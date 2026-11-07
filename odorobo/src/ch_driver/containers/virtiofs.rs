//! Actor-owned virtiofsd supervision.

use std::path::PathBuf;
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
        let task_state = Arc::clone(&state);
        let task_spec = spec.clone();

        let handle = tokio::spawn(async move {
            let mut backoff_secs: u64 = 1;
            loop {
                if task_state.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                task_state.ready.store(false, Ordering::Relaxed);
                // Only the previous child has owned this private VM socket.
                // Remove it after exit, before each replacement generation.
                std::fs::remove_file(&task_spec.socket).ok();
                let log = match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&task_spec.log)
                {
                    Ok(log) => log,
                    Err(err) => {
                        warn!(
                            error = ?err,
                            log = %task_spec.log.display(),
                            "cannot open virtiofsd log; stopping supervisor"
                        );
                        break;
                    }
                };
                let mut cmd = Command::new(&task_spec.program);
                cmd.args([
                    format!("--socket-path={}", task_spec.socket.display()),
                    format!("--shared-dir={}", task_spec.shared_dir.display()),
                    "--sandbox=chroot".to_owned(),
                    "--cache=auto".to_owned(),
                    "--xattr".to_owned(),
                    "--posix-acl".to_owned(),
                ])
                .stdin(std::process::Stdio::null())
                .stdout(log.try_clone().expect("log handle clone"))
                .stderr(log)
                .kill_on_drop(true);

                debug!(program = %task_spec.program.display(), "spawning virtiofsd");
                match cmd.spawn() {
                    Ok(mut child) => {
                        let exit = loop {
                            match child.try_wait() {
                                Ok(Some(status)) => break Ok(status),
                                Err(err) => break Err(err),
                                Ok(None) => {}
                            }
                            // A pathname from an exited generation is never
                            // readiness. Only the current live child publishes it.
                            task_state
                                .ready
                                .store(task_spec.socket.exists(), Ordering::Relaxed);
                            tokio::select! {
                                () = tokio::time::sleep(Duration::from_millis(50)) => {}
                                () = task_state.notify.notified() => {
                                    _ = child.start_kill();
                                    break child.wait().await;
                                }
                            }
                        };
                        task_state.ready.store(false, Ordering::Relaxed);
                        std::fs::remove_file(&task_spec.socket).ok();
                        if task_state.shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        warn!(?exit, "virtiofsd exited unexpectedly, restarting");
                    }
                    Err(err) => {
                        warn!(
                            error = ?err,
                            program = %task_spec.program.display(),
                            "failed to spawn virtiofsd"
                        );
                    }
                }
                let restarts = task_state.restarts.fetch_add(1, Ordering::Relaxed);
                warn!(
                    restarts,
                    backoff_secs, "virtiofsd backing off before restart"
                );
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                    () = task_state.notify.notified() => {}
                }
                backoff_secs = backoff_secs.saturating_mul(2).min(30);
            }
            debug!("virtiofsd supervisor loop exiting");
        });

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[tokio::test]
    async fn supervisor_replaces_stale_socket_generation() {
        let base =
            std::env::temp_dir().join(format!("odorobo-socket-restart-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base).unwrap();
        let script = base.join("stub");
        std::fs::write(&script, "#!/usr/bin/python3\nimport socket,sys,time,pathlib\np=pathlib.Path(__file__).parent\ns=socket.socket(socket.AF_UNIX)\ns.bind(str(p/'sock'))\nf=p/'generation'\nn=int(f.read_text())+1 if f.exists() else 1\nf.write_text(str(n))\ntime.sleep(0.15 if n==1 else 100)\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let supervisor = VirtioFsSupervisor::start(VirtiofsdSpec {
            program: script,
            socket: base.join("sock"),
            shared_dir: base.clone(),
            log: base.join("log"),
        });
        supervisor
            .wait_socket_ready(Duration::from_secs(2))
            .await
            .unwrap();
        for _ in 0..100 {
            if supervisor.restarts() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(supervisor.restarts() > 0);
        assert!(
            !supervisor
                .state
                .ready
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        supervisor
            .wait_socket_ready(Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(base.join("generation")).unwrap(),
            "2"
        );
        supervisor.stop().await;
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn supervisor_timeout_aborts_and_joins_task() {
        let state = std::sync::Arc::new(super::SupState {
            shutdown: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
            restarts: std::sync::atomic::AtomicU32::new(0),
            ready: std::sync::atomic::AtomicBool::new(false),
        });
        let task = tokio::spawn(std::future::pending::<()>());
        let abort = task.abort_handle();
        let supervisor = VirtioFsSupervisor {
            spec: VirtiofsdSpec {
                program: "/bin/false".into(),
                socket: "unused".into(),
                shared_dir: "unused".into(),
                log: "unused".into(),
            },
            state,
            task: tokio::sync::Mutex::new(Some(task)),
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
    async fn supervisor_restarts_dead_processes_and_stops_cleanly() {
        let base = std::env::temp_dir().join(format!("odorobo-sup-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base).unwrap();
        let supervisor = VirtioFsSupervisor::start(VirtiofsdSpec {
            // /bin/false exits instantly -> the supervisor should keep
            // restarting it with backoff.
            program: "/bin/false".into(),
            socket: base.join("sock"),
            shared_dir: base.clone(),
            log: base.join("log"),
        });

        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            supervisor.restarts() >= 1,
            "dead process should be restarted"
        );
        assert!(!base.join("sock").exists(), "no socket from /bin/false");

        supervisor.stop().await;
        let restarts_at_stop = supervisor.restarts();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            supervisor.restarts(),
            restarts_at_stop,
            "no restarts after stop"
        );

        // Use a script that accepts virtiofsd's arguments and execs a real
        // long-running child. /bin/sleep rejects those arguments immediately.
        let base2 = std::env::temp_dir().join(format!("odorobo-sup2-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base2).unwrap();
        let script = base2.join("virtiofsd-stub");
        std::fs::write(
            &script,
            "#!/bin/sh\necho $$ > \"$(dirname \"$0\")/pid\"\nexec sleep 100\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let supervisor = VirtioFsSupervisor::start(VirtiofsdSpec {
            program: script.clone(),
            socket: base2.join("sock"),
            shared_dir: base2.clone(),
            log: base2.join("log"),
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        supervisor.stop().await;
        let pid: u32 = std::fs::read_to_string(base2.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child leaked after stop"
        );
        let supervisor = VirtioFsSupervisor::start(VirtiofsdSpec {
            program: script,
            socket: base2.join("sock"),
            shared_dir: base2.clone(),
            log: base2.join("log"),
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let pid: u32 = std::fs::read_to_string(base2.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(Path::new(&format!("/proc/{pid}")).exists());
        drop(supervisor);
        for _ in 0..100 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child leaked after drop"
        );

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&base2).ok();
    }
}
