//! Actor-owned virtiofsd supervision.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use stable_eyre::{Result, eyre::bail};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, warn};

use super::ROOTFS_TAG;

/// vhost-user socket path for a VM's rootfs device. Single source of truth
/// shared with the manifest conversion's `FsConfig` so the actor's virtiofsd
/// supervisor and the CH fs device config always agree.
pub fn socket_path_for(vmid: &str) -> PathBuf {
    crate::ch_driver::VMInstance::runtime_dir_for(vmid).join(format!("{ROOTFS_TAG}.sock"))
}

/// Resolve the virtiofsd binary (Fedora ships it in /usr/libexec, not PATH).
pub fn virtiofsd_path() -> Option<PathBuf> {
    if let Ok(output) = std::process::Command::new("which")
        .arg("virtiofsd")
        .output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let libexec = PathBuf::from("/usr/libexec/virtiofsd");
    libexec.exists().then_some(libexec)
}

/// Everything needed to (re)spawn one virtiofsd instance.
#[derive(Debug, Clone)]
pub struct VirtiofsdSpec {
    /// Binary to run (normally `virtiofsd_path()`; override in tests).
    pub program: PathBuf,
    /// vhost-user socket the Cloud Hypervisor process connects to.
    pub socket: PathBuf,
    /// Directory (the composefs mount) served to the guest.
    pub shared_dir: PathBuf,
    /// Log file for virtiofsd stdout/stderr.
    pub log: PathBuf,
    /// Deny the guest every write into the share (an ephemeral root writes to
    /// its own tmpfs upper instead).
    pub read_only: bool,
}

pub(super) struct SupState {
    pub shutdown: AtomicBool,
    pub notify: Notify,
    pub restarts: AtomicU32,
    pub ready: AtomicBool,
}

/// Runs virtiofsd as a supervised child of the VM actor for the VM's
/// lifetime: restarts with capped backoff on unexpected exit, stopped with
/// the actor. Children are `kill_on_drop`, so even an abrupt actor kill does
/// not leak the process.
pub struct VirtioFsSupervisor {
    spec: VirtiofsdSpec,
    state: Arc<SupState>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    abort: tokio::task::AbortHandle,
}

impl Drop for VirtioFsSupervisor {
    fn drop(&mut self) {
        // Dropping a JoinHandle alone detaches it. Abort the task so its
        // kill_on_drop child cannot outlive a cancelled startup or actor.
        self.state.shutdown.store(true, Ordering::Relaxed);
        self.abort.abort();
    }
}

impl VirtioFsSupervisor {
    /// Start supervising a virtiofsd instance for this spec.
    pub fn start(spec: VirtiofsdSpec) -> Self {
        // A stale socket from a previous run would make CH connect to
        // nothing; virtiofsd also refuses to replace an existing socket.
        std::fs::remove_file(&spec.socket).ok();

        let state = Arc::new(SupState {
            shutdown: AtomicBool::new(false),
            notify: Notify::new(),
            restarts: AtomicU32::new(0),
            ready: AtomicBool::new(false),
        });
        let handle = tokio::spawn(supervise_virtiofsd(spec.clone(), Arc::clone(&state)));

        Self {
            spec,
            state,
            abort: handle.abort_handle(),
            task: Mutex::new(Some(handle)),
        }
    }

    /// Number of unexpected exits / failed spawns since start
    /// (observability and tests).
    pub fn restarts(&self) -> u32 {
        self.state.restarts.load(Ordering::Relaxed)
    }

    /// Wait until the vhost-user socket exists (virtiofsd creates it at
    /// startup) so CH can attach the fs device.
    pub async fn wait_socket_ready(&self, timeout: Duration) -> Result<()> {
        let started = tokio::time::Instant::now();
        loop {
            if self.state.ready.load(Ordering::Relaxed) {
                return Ok(());
            }
            if started.elapsed() >= timeout {
                bail!(
                    "virtiofsd socket {} did not appear within {timeout:?} — see {}",
                    self.spec.socket.display(),
                    self.spec.log.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Stop supervising and kill the running child, if any.
    pub async fn stop(&self) {
        self.stop_with_timeout(Duration::from_secs(5)).await;
    }

    pub(super) async fn stop_with_timeout(&self, timeout: Duration) {
        self.state.shutdown.store(true, Ordering::Relaxed);
        self.state.notify.notify_one();
        // Serialize concurrent stop calls until the child has been reaped.
        let mut slot = self.task.lock().await;
        if let Some(task) = slot.as_mut() {
            // Give the loop a moment to observe the shutdown flag and reap
            // the child; abort (kill_on_drop) as a backstop.
            if tokio::time::timeout(timeout, &mut *task).await.is_err() {
                warn!("virtiofsd supervisor task did not exit in time; aborting");
                task.abort();
                _ = task.await;
            }
            *slot = None;
        }
    }
}

/// Spawn, reap and restart virtiofsd until the shutdown flag is set.
async fn supervise_virtiofsd(spec: VirtiofsdSpec, state: Arc<SupState>) {
    // Backoff doubles from one second up to 30 so a crash loop stays cheap.
    let mut backoff_secs: u64 = 1;
    loop {
        if state.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let Some(mut cmd) = virtiofsd_command(&spec) else {
            break;
        };
        debug!(program = %spec.program.display(), "spawning virtiofsd");
        match cmd.spawn() {
            Ok(child) => {
                let exit = wait_for_exit(child, &state, &spec.socket).await;
                if state.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                warn!(?exit, "virtiofsd exited unexpectedly, restarting");
            }
            Err(err) => {
                warn!(
                    error = ?err,
                    program = %spec.program.display(),
                    "failed to spawn virtiofsd"
                );
            }
        }
        let restarts = state.restarts.fetch_add(1, Ordering::Relaxed);
        warn!(
            restarts,
            backoff_secs, "virtiofsd backing off before restart"
        );
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
            () = state.notify.notified() => {}
        }
        backoff_secs = backoff_secs.saturating_mul(2).min(30);
    }
    debug!("virtiofsd supervisor loop exiting");
}

/// Build the virtiofsd command with its log attached; `None` means the log
/// cannot be opened and the supervisor must stop.
fn virtiofsd_command(spec: &VirtiofsdSpec) -> Option<Command> {
    let log = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log)
    {
        Ok(log) => log,
        Err(err) => {
            warn!(
                error = ?err,
                log = %spec.log.display(),
                "cannot open virtiofsd log; stopping supervisor"
            );
            return None;
        }
    };
    let mut args = vec![
        format!("--socket-path={}", spec.socket.display()),
        format!("--shared-dir={}", spec.shared_dir.display()),
        "--sandbox=namespace".to_owned(),
        "--cache=auto".to_owned(),
        "--xattr".to_owned(),
        "--posix-acl".to_owned(),
    ];
    if spec.read_only {
        args.push("--readonly".to_owned());
    }
    let mut cmd = Command::new(&spec.program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().expect("log handle clone"))
        .stderr(log)
        .kill_on_drop(true);
    Some(cmd)
}

/// Reap the child and drop its socket generation, so the next one starts
/// clean and ready only describes the live child.
async fn wait_for_exit(
    mut child: tokio::process::Child,
    state: &SupState,
    socket: &Path,
) -> std::io::Result<std::process::ExitStatus> {
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(err) => break Err(err),
            Ok(None) => {}
        }
        // A pathname from an exited generation is never readiness.
        state.ready.store(socket.exists(), Ordering::Relaxed);
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
            () = state.notify.notified() => {
                _ = child.start_kill();
                break child.wait().await;
            }
        }
    };
    state.ready.store(false, Ordering::Relaxed);
    std::fs::remove_file(socket).ok();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Scratch directory removed on drop, including when the test panics.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(prefix: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("odorobo-{prefix}-{}", ulid::Ulid::generate()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// Poll `condition` every 10 ms for a second, then fail with `description`.
    async fn eventually(description: &str, mut condition: impl FnMut() -> bool) {
        for _ in 0..100 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {description}");
    }

    /// Write an executable stub and return its path.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    /// Spec whose socket, share and log all live in `dir`.
    fn spec_in(dir: &Path, program: PathBuf) -> VirtiofsdSpec {
        VirtiofsdSpec {
            program,
            socket: dir.join("sock"),
            shared_dir: dir.to_path_buf(),
            log: dir.join("log"),
            read_only: false,
        }
    }

    fn read_pid(path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn process_alive(pid: u32) -> bool {
        Path::new("/proc").join(pid.to_string()).exists()
    }

    #[tokio::test]
    async fn read_only_specs_pass_the_virtiofsd_readonly_flag() {
        let dir = TestDir::new("sup-ro");
        let program = script(
            dir.path(),
            "virtiofsd-stub",
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/args\"\nexec sleep 100\n",
        );
        let mut spec = spec_in(dir.path(), program);
        spec.read_only = true;
        let supervisor = VirtioFsSupervisor::start(spec);

        let args = dir.path().join("args");
        eventually("virtiofsd arguments to be recorded", || args.exists()).await;
        let args = std::fs::read_to_string(&args).unwrap();
        assert!(args.lines().any(|arg| arg == "--readonly"), "{args}");
        assert!(
            args.lines().any(|arg| arg == "--sandbox=namespace"),
            "the daemon must be confined to its own namespaces: {args}"
        );
        supervisor.stop().await;
    }

    #[tokio::test]
    async fn supervisor_replaces_stale_socket_generation() {
        let dir = TestDir::new("socket-restart");
        // Only the socket pathname matters here, so a touched file stands in
        // for a bound socket; no guest connects in this test.
        let program = script(
            dir.path(),
            "stub",
            r#"#!/bin/sh
dir=$(dirname "$0")
count=$(( $(cat "$dir/generation" 2>/dev/null || echo 0) + 1 ))
printf '%s' "$count" > "$dir/generation"
: > "$dir/sock"
if [ "$count" -eq 1 ]; then sleep 0.15; else sleep 100; fi
"#,
        );
        let supervisor = VirtioFsSupervisor::start(spec_in(dir.path(), program));

        supervisor
            .wait_socket_ready(Duration::from_secs(2))
            .await
            .unwrap();
        eventually("the first generation to exit", || supervisor.restarts() > 0).await;
        assert!(!supervisor.state.ready.load(Ordering::Relaxed));
        supervisor
            .wait_socket_ready(Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("generation")).unwrap(),
            "2"
        );
        supervisor.stop().await;
    }

    #[tokio::test]
    async fn supervisor_timeout_aborts_and_joins_task() {
        let task = tokio::spawn(std::future::pending::<()>());
        let abort = task.abort_handle();
        let supervisor = VirtioFsSupervisor {
            spec: spec_in(Path::new("unused"), "/bin/false".into()),
            state: Arc::new(SupState {
                shutdown: AtomicBool::new(false),
                notify: Notify::new(),
                restarts: AtomicU32::new(0),
                ready: AtomicBool::new(false),
            }),
            task: Mutex::new(Some(task)),
            abort: abort.clone(),
        };
        supervisor.stop_with_timeout(Duration::from_millis(1)).await;
        assert!(
            abort.is_finished(),
            "timeout must not detach the supervisor"
        );
        supervisor.stop().await;
    }

    #[tokio::test]
    async fn supervisor_restarts_dead_processes_and_stop_is_final() {
        let dir = TestDir::new("sup-restart");
        // /bin/false rejects virtiofsd's arguments and exits instantly.
        let supervisor = VirtioFsSupervisor::start(spec_in(dir.path(), "/bin/false".into()));

        eventually("the dead process to be restarted", || {
            supervisor.restarts() >= 1
        })
        .await;
        assert!(
            !dir.path().join("sock").exists(),
            "no socket from /bin/false"
        );

        supervisor.stop().await;
        let restarts_at_stop = supervisor.restarts();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            supervisor.restarts(),
            restarts_at_stop,
            "no restarts after stop"
        );
    }

    #[tokio::test]
    async fn supervisor_stops_and_drops_the_child() {
        let dir = TestDir::new("sup-child");
        let program = script(
            dir.path(),
            "virtiofsd-stub",
            "#!/bin/sh\necho $$ > \"$(dirname \"$0\")/pid\"\nexec sleep 100\n",
        );
        let spec = spec_in(dir.path(), program);
        let pid_file = dir.path().join("pid");

        let supervisor = VirtioFsSupervisor::start(spec.clone());
        eventually("the stub to record its pid", || pid_file.exists()).await;
        let pid = read_pid(&pid_file);
        supervisor.stop().await;
        eventually("the stopped child to exit", || !process_alive(pid)).await;

        std::fs::remove_file(&pid_file).unwrap();
        let supervisor = VirtioFsSupervisor::start(spec);
        eventually("the restarted stub to record its pid", || pid_file.exists()).await;
        let pid = read_pid(&pid_file);
        assert!(process_alive(pid));
        drop(supervisor);
        eventually("the dropped child to exit", || !process_alive(pid)).await;
    }
}
