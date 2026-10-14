//! External tool execution with bounded output, timeouts, and process-group
//! cleanup that survives caller cancellation.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::time::Duration;

use stable_eyre::{
    Result,
    eyre::{Context, bail, eyre},
};
use tokio::process::{Child, Command};

pub(super) fn set_child_file_size_limit(command: &mut Command, limit_bytes: u64) -> Result<()> {
    #[allow(
        clippy::useless_conversion,
        reason = "rlim_t is not u64 on every target"
    )]
    let limit: libc::rlim_t = limit_bytes
        .try_into()
        .map_err(|_| eyre!("file size limit exceeds RLIMIT_FSIZE"))?;
    // SAFETY: the child hook calls only setrlimit before exec and captures a
    // plain scalar; it does not access Tokio state or allocate in the child.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            let limits = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &raw const limits) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    };
    Ok(())
}

/// Held by every [`Tool::ORDERED`] command for its whole lifetime, including
/// after a cancelled caller has gone. Teardown takes it as a barrier so it can
/// never inspect mounts or RBD mappings before an in-flight change completes.
pub(super) static TOOL_OPERATIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How an external tool runs. Every tool runs in its own process group inside
/// a detached task that owns the child, so a cancelled caller cannot orphan
/// it; on timeout the whole group is killed and the child reaped first.
#[derive(Clone, Copy, Debug)]
pub(super) struct Tool {
    timeout: Option<Duration>,
    serialize: bool,
}

impl Tool {
    /// Mount table and RBD mapping changes (and RBD state reads that must
    /// observe them): serialized on [`TOOL_OPERATIONS`] with a short timeout.
    pub(super) const ORDERED: Self = Self {
        timeout: Some(Duration::from_secs(120)),
        serialize: true,
    };
    /// Work that scales with data size (composefs builds, fs-verity, mkfs,
    /// fsck). Callers order it with their own locks; it must never stall
    /// other VMs' mount/RBD teardown. The generous deadline only catches a
    /// wedged tool (which would otherwise hold its caller's lock forever);
    /// data volume is already bounded by the node size limits.
    pub(super) const BULK: Self = Self {
        timeout: Some(Duration::from_hours(1)),
        serialize: false,
    };

    /// Unserialized command with a deadline, for queries.
    pub(super) const fn bounded(timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
            serialize: false,
        }
    }

    /// Run with piped stdout/stderr. Waiting for pipe EOF also waits for
    /// helper descendants that inherited them.
    pub(super) async fn output(self, mut command: Command) -> Result<std::process::Output> {
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        self.own(command, Child::wait_with_output).await
    }

    /// Run with the caller's stdio configuration.
    pub(super) async fn status(self, command: Command) -> Result<std::process::ExitStatus> {
        self.own(command, |mut child| async move { child.wait().await })
            .await
    }

    /// Run `program args.. extra..`, failing on a non-zero exit; returns stdout.
    pub(super) async fn run(
        self,
        program: &str,
        args: impl IntoIterator<Item = String>,
        extra: &[&Path],
    ) -> Result<String> {
        let mut cmd = Command::new(program);
        cmd.args(args).args(extra);
        let output = self
            .output(cmd)
            .await
            .wrap_err_with(|| format!("failed to run {program}"))?;
        if !output.status.success() {
            bail!(
                "{program} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn own<T, F>(
        self,
        mut command: Command,
        wait: impl FnOnce(Child) -> F + Send + 'static,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: Future<Output = std::io::Result<T>> + Send + 'static,
    {
        let guard = if self.serialize {
            Some(TOOL_OPERATIONS.lock().await)
        } else {
            None
        };
        command.kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let child = command
            .spawn()
            .wrap_err("failed to execute external tool")?;
        let task = tokio::spawn(async move {
            let _guard = guard;
            let pid = child
                .id()
                .ok_or_else(|| eyre!("external tool child has no PID"))?;
            let wait = wait(child);
            tokio::pin!(wait);
            let Some(limit) = self.timeout else {
                return wait.await.wrap_err("external tool operation failed");
            };
            if let Ok(result) = tokio::time::timeout(limit, &mut wait).await {
                return result.wrap_err("external tool operation failed");
            }
            // SAFETY: child is still owned by the pinned wait future and
            // unreaped; its PID cannot be reused before that future ends.
            unsafe { libc::killpg(i32::try_from(pid)?, libc::SIGKILL) };
            _ = wait.await;
            bail!(
                "external tool timed out after {}s; process group killed and child reaped",
                limit.as_secs()
            );
        });
        task.await
            .map_err(|err| eyre!("external tool task failed: {err}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ch_driver::containers::test_support::{TempDir, sh_in, wait_for_path};

    #[tokio::test]
    async fn cancelled_rbd_caller_cannot_overtake_its_command() {
        let base = TempDir::new("rbd-cancel");
        let caller = tokio::spawn(Tool::ORDERED.output(sh_in(
            &base,
            "echo $$ > \"$1/pid\"; sleep 0.2; echo mapped > \"$1/mapped\"",
        )));
        wait_for_path(&base.join("pid")).await;
        caller.abort();
        _ = caller.await;
        // Reconciliation must wait for the detached owned command, then sees
        // its effects; no mapping can appear after cleanup has inspected them.
        let guard = TOOL_OPERATIONS.lock().await;
        assert!(base.join("mapped").exists());
        let pid: u32 = std::fs::read_to_string(base.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        std::fs::remove_file(base.join("mapped")).unwrap();
        drop(guard);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!base.join("mapped").exists());
    }

    #[tokio::test]
    async fn bulk_tools_do_not_wait_for_ordered_operations() {
        let _guard = TOOL_OPERATIONS.lock().await;
        tokio::time::timeout(Duration::from_secs(10), Tool::BULK.run("true", [], &[]))
            .await
            .expect("bulk tool must not serialize behind mount/RBD operations")
            .unwrap();
    }

    #[tokio::test]
    async fn timed_out_tool_kills_its_process_group() {
        let base = TempDir::new("tool-timeout");
        let err = Tool::bounded(Duration::from_millis(200))
            .output(sh_in(&base, "sleep 30 & echo $! > \"$1/pid\"; wait"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err:?}");
        let pid = std::fs::read_to_string(base.join("pid")).unwrap();
        let stat =
            std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())).unwrap_or_default();
        // The orphaned descendant is either gone or a zombie awaiting init.
        assert!(
            stat.is_empty() || stat.contains(") Z "),
            "descendant survived: {stat}"
        );
    }
}
