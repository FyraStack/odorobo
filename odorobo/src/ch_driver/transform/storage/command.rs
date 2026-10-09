//! Bounded CLI execution for storage backends. A killed CLI may already have
//! submitted work to a daemon/kernel, so errors after spawn are always uncertain.
use super::super::StorageAcquisition;
use stable_eyre::{Report, Result, eyre::eyre};
use std::{process::Output, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const REAP_TIMEOUT: Duration = Duration::from_secs(2);
const OUTPUT_LIMIT: usize = 64 * 1024;

pub(in crate::ch_driver::transform::storage) enum CommandFailure {
    BeforeSpawn(Report),
    Uncertain(Report),
}

impl CommandFailure {
    pub(in crate::ch_driver::transform::storage) fn into_report(self) -> Report {
        match self {
            Self::BeforeSpawn(error) | Self::Uncertain(error) => error,
        }
    }

    pub(in crate::ch_driver::transform::storage) fn into_release_report(self) -> Report {
        match self {
            Self::BeforeSpawn(error) => error,
            Self::Uncertain(error) => super::super::uncertain_release(error),
        }
    }

    pub(in crate::ch_driver::transform::storage) fn into_acquisition(
        self,
    ) -> Result<StorageAcquisition> {
        match self {
            Self::BeforeSpawn(error) => Err(error),
            // An immediate absence check is not enough: an in-flight daemon or
            // kernel request can finish after the CLI has been killed/reaped.
            Self::Uncertain(error) => Ok(StorageAcquisition::uncertain(error)),
        }
    }
}

struct RunningCommand {
    child: Option<Child>,
    group: i32,
}

impl RunningCommand {
    fn terminate(&mut self) {
        // The CLI's helpers may inherit its output pipes. Kill the isolated
        // process group, not just the leader, before attempting a bounded reap.
        // SAFETY: spawn configured a new group whose id is this child's PID;
        // the strictly positive id can never address our own process group.
        unsafe {
            libc::kill(
                self.group.checked_neg().expect("positive child PID"),
                libc::SIGKILL,
            )
        };
        if let Some(child) = &mut self.child {
            _ = child.start_kill();
        }
    }

    async fn kill_and_reap(&mut self) {
        self.terminate();
        let child = self.child.as_mut().expect("running child");
        if let Ok(Ok(_)) = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await {
            self.child.take();
        } else {
            // A process stuck in an uninterruptible kernel operation may not
            // reap promptly. Keep a background waiter rather than block storage
            // forever. kill_on_drop also covers runtime shutdown/cancellation.
            let mut child = self.child.take().expect("running child");
            tokio::spawn(async move {
                _ = child.wait().await;
            });
        }
    }
}

impl Drop for RunningCommand {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.terminate();
            // Tokio's kill_on_drop and orphan reaper cover cancellation while
            // either execution or bounded cleanup is awaiting.
        }
    }
}

async fn capture(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = vec![0; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return if truncated {
                // In particular, a truncated `rbd device list` must not silently
                // hide an existing mapping and lead to a false ownership claim.
                Err(std::io::Error::other("storage CLI output exceeded limit"))
            } else {
                Ok(output)
            };
        }
        // Continue draining after the cap so a verbose CLI cannot deadlock on
        // its pipes, without allowing unbounded memory use.
        let keep = count.min(OUTPUT_LIMIT.saturating_sub(output.len()));
        truncated |= keep != count;
        output.extend_from_slice(&buffer[..keep]);
    }
}

pub(in crate::ch_driver::transform::storage) async fn run(
    command: &mut Command,
    operation: &str,
) -> std::result::Result<Output, CommandFailure> {
    run_with_timeout(command, operation, COMMAND_TIMEOUT).await
}

async fn run_with_timeout(
    command: &mut Command,
    operation: &str,
    timeout: Duration,
) -> std::result::Result<Output, CommandFailure> {
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()
        .map_err(|error| {
            CommandFailure::BeforeSpawn(eyre!("Failed to execute {operation}: {error}"))
        })?;
    let group =
        i32::try_from(child.id().expect("spawned child PID")).expect("kernel PID must fit pid_t");
    let mut running = RunningCommand {
        child: Some(child),
        group,
    };
    let child = running.child.as_mut().expect("running child");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let output = tokio::time::timeout(timeout, async {
        let (stdout, stderr) = tokio::try_join!(capture(stdout), capture(stderr))?;
        // Reap after draining pipes. If a helper inherits them, retain the
        // leader's PID until group termination, preventing PID/group-id reuse.
        let status = child.wait().await?;
        Ok::<_, std::io::Error>(Output {
            status,
            stdout,
            stderr,
        })
    })
    .await;
    let error = match output {
        Ok(Ok(output)) => {
            // Successfully reaped; disarm the cancellation guard.
            running.child.take();
            return Ok(output);
        }
        Ok(Err(error)) => eyre!("Failed to wait for {operation}: {error}; outcome is uncertain"),
        Err(_) => eyre!("{operation} timed out after {timeout:?}; outcome is uncertain"),
    };
    running.kill_and_reap().await;
    Err(CommandFailure::Uncertain(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_failure_has_no_acquisition() {
        let error = run(
            &mut Command::new("/nonexistent/odorobo-storage-cli"),
            "test spawn",
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(error, CommandFailure::BeforeSpawn(_)));
        drop(
            error
                .into_acquisition()
                .err()
                .expect("spawn failure must not acquire"),
        );
    }

    #[test]
    fn only_post_spawn_release_failures_carry_uncertainty_marker() {
        let before = CommandFailure::BeforeSpawn(eyre!("spawn failed")).into_release_report();
        assert!(
            before
                .downcast_ref::<super::super::super::StorageReleaseUncertain>()
                .is_none()
        );
        for message in ["logout timed out", "unmap wait failed"] {
            let after = CommandFailure::Uncertain(eyre!(message)).into_release_report();
            assert!(
                after
                    .downcast_ref::<super::super::super::StorageReleaseUncertain>()
                    .is_some()
            );
            assert!(after.to_string().contains(message));
        }
    }

    #[tokio::test]
    async fn captures_success_and_nonzero_exit_without_inferring_ownership() {
        for code in [0, 15] {
            let output = run(
                Command::new("sh")
                    .arg("-c")
                    .arg(format!("printf stdout; printf stderr >&2; exit {code}")),
                "test exit",
            )
            .await
            .unwrap_or_else(|error| panic!("{}", error.into_report()));
            assert_eq!(output.status.code(), Some(code));
            assert_eq!(output.stdout, b"stdout");
            assert_eq!(output.stderr, b"stderr");
        }
    }

    #[tokio::test]
    async fn timeout_kills_and_reaps_cli_and_preserves_uncertainty() {
        let pid_path = std::env::temp_dir().join(format!(
            "odorobo-storage-command-{}",
            ulid::Ulid::generate()
        ));
        let script = format!("echo $$ > '{}'; exec sleep 60", pid_path.display());
        let start = std::time::Instant::now();
        let error = run_with_timeout(
            Command::new("sh").arg("-c").arg(script),
            "test timeout",
            Duration::from_millis(200),
        )
        .await
        .err()
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(matches!(error, CommandFailure::Uncertain(_)));
        let acquisition = error.into_acquisition().unwrap();
        assert!(matches!(
            acquisition.ownership,
            super::super::super::StorageOwnership::Uncertain(_)
        ));
        let pid: i32 = tokio::fs::read_to_string(&pid_path)
            .await
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SIGKILL is not sufficient on its own: the leader must also be reaped.
        // SAFETY: signal zero only queries existence of the test child PID.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        tokio::fs::remove_file(pid_path).await.unwrap();
    }

    #[tokio::test]
    async fn descendant_holding_pipes_cannot_defeat_deadline() {
        let start = std::time::Instant::now();
        let error = run_with_timeout(
            Command::new("sh").arg("-c").arg("sleep 60 & exit 0"),
            "test inherited pipes",
            Duration::from_millis(200),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(error, CommandFailure::Uncertain(_)));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn rejects_truncated_output_rather_than_hide_mappings() {
        let error = run(
            Command::new("sh").arg("-c").arg("head -c 262144 /dev/zero"),
            "test output cap",
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(error, CommandFailure::Uncertain(_)));
        assert!(
            error
                .into_report()
                .to_string()
                .contains("output exceeded limit")
        );
    }
}
