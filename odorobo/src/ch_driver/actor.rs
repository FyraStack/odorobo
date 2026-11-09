use std::{collections::VecDeque, sync::Arc};

use crate::messages::vm::{
    DeleteVM, DeleteVMReply, GetConsoleHistory, GetConsoleHistoryReply, GetVMHeartbeat,
    GetVMHeartbeatReply, GetVMInfo, GetVMInfoReply, GetWorkloadLogs, GetWorkloadLogsReply,
    MigrateVMReceive, MigrateVMReceiveReply, PrepMigration, SendConsoleInput,
    SendConsoleInputReply, ShutdownVM,
};
use crate::{
    ch_driver::{
        VMInstance,
        containers::{PreparedRootfs, VirtioFsSupervisor, VirtiofsdSpec},
        manifest::to_vm_config,
        transform::{ConsoleTransform, TransformChain},
    },
    manifest::VmManifest,
    networking::{actor::NetworkAgentActor, messages::PublishPorts},
};
use cloud_hypervisor_client::models::VmConfig;
use kameo::prelude::*;
use serde::{Deserialize, Serialize};
use stable_eyre::{Report, Result, eyre::eyre};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixStream, unix::OwnedWriteHalf},
    sync::{Mutex, broadcast},
    task::JoinHandle,
};
use tracing::{debug, error, info, trace, warn};

/// Cloud Hypervisor-specific state for an in-progress receive migration.
///
/// The public migration messages carry `VmManifest`; the translated `VmConfig`
/// stays private to the CH actor because another backend could use different
/// migration metadata.
pub struct MigrationState {
    pub listening_address: String,
    pub config: VmConfig,
    /// The task handle for the migration process.
    pub migration_task: Option<JoinHandle<()>>,
}

const CONSOLE_SPOOL_SIZE: usize = 1024 * 1024;

/// Bounded serial-console history shared with the task draining the CH socket.
#[derive(Clone)]
pub struct Console {
    inner: Arc<Mutex<ConsoleBuffer>>,
    output: broadcast::Sender<Vec<u8>>,
    writer: Arc<Mutex<Option<OwnedWriteHalf>>>,
}

impl Default for Console {
    fn default() -> Self {
        let (output, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Mutex::new(ConsoleBuffer::default())),
            output,
            writer: Arc::new(Mutex::new(None)),
        }
    }
}

#[derive(Default)]
struct ConsoleBuffer {
    ring: VecDeque<Vec<u8>>,
    len: usize,
}

impl Console {
    /// Attach to a Cloud Hypervisor serial socket and start spooling its output.
    pub async fn attach_socket(&self, socket_path: std::path::PathBuf) -> Result<()> {
        let stream = UnixStream::connect(&socket_path).await.map_err(|err| {
            Report::msg(format!(
                "failed to attach console spool to {}: {err}",
                socket_path.display()
            ))
        })?;
        let (mut reader, writer) = stream.into_split();
        let mut writer_guard = self.writer.lock().await;
        if writer_guard.is_some() {
            return Ok(());
        }
        *writer_guard = Some(writer);
        drop(writer_guard);

        let spool = self.clone();
        tokio::spawn(async move {
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => {
                        debug!("serial console closed");
                        break;
                    }
                    Ok(read) => spool.push(buffer[..read].to_vec()).await,
                    Err(err) => {
                        warn!(?err, "serial console spool stopped reading");
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    async fn push(&self, chunk: Vec<u8>) {
        trace!(
            bytes = chunk.len(),
            output = %String::from_utf8_lossy(&chunk),
            "serial console output received"
        );
        let _subscribers = self.output.send(chunk.clone());
        let chunk = if chunk.len() > CONSOLE_SPOOL_SIZE {
            chunk[chunk.len().saturating_sub(CONSOLE_SPOOL_SIZE)..].to_vec()
        } else {
            chunk
        };
        {
            let mut buffer = self.inner.lock().await;
            buffer.len = buffer.len.saturating_add(chunk.len());
            buffer.ring.push_back(chunk);
            while buffer.len > CONSOLE_SPOOL_SIZE {
                let excess = buffer.len.saturating_sub(CONSOLE_SPOOL_SIZE);
                if let Some(oldest) = buffer.ring.pop_front() {
                    if oldest.len() > excess {
                        buffer.len = buffer.len.saturating_sub(excess);
                        buffer.ring.push_front(oldest[excess..].to_vec());
                    } else {
                        buffer.len = buffer.len.saturating_sub(oldest.len());
                    }
                } else {
                    buffer.len = 0;
                    break;
                }
            }
            drop(buffer);
        }
    }

    /// Subscribe to live serial output. Chunks are broadcast without replay.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output.subscribe()
    }

    /// Write input bytes to the guest serial console.
    pub async fn write_input(&self, input: &[u8]) -> Result<()> {
        {
            let mut writer_guard = self.writer.lock().await;
            let writer = writer_guard
                .as_mut()
                .ok_or_else(|| Report::msg("console is not attached"))?;
            let result = writer
                .write_all(input)
                .await
                .map_err(|err| Report::msg(format!("failed to write to serial console: {err}")));
            drop(writer_guard);
            result
        }
    }

    /// Whether a console spool is attached to a VM socket.
    pub async fn is_attached(&self) -> bool {
        self.writer.lock().await.is_some()
    }

    /// Return the currently retained output, oldest bytes first.
    pub async fn history(&self) -> Vec<u8> {
        let mut history = Vec::new();
        {
            let buffer = self.inner.lock().await;
            history.reserve(buffer.len);
            for chunk in &buffer.ring {
                history.extend_from_slice(chunk);
            }
            drop(buffer);
        };
        history
    }
}

#[cfg(test)]
mod tests {
    use super::{CONSOLE_SPOOL_SIZE, Console};

    #[tokio::test]
    async fn console_history_is_bounded_to_one_megabyte() {
        let console = Console::default();
        console.push(vec![b'a'; CONSOLE_SPOOL_SIZE]).await;
        console.push(b"tail".to_vec()).await;

        let history = console.history().await;
        assert_eq!(history.len(), CONSOLE_SPOOL_SIZE);
        assert_eq!(&history[..4], b"aaaa");
        assert_eq!(&history[CONSOLE_SPOOL_SIZE - 4..], b"tail");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_oci_start_keeps_a_deletable_owner() {
        use crate::manifest::{Compute, DesiredState, Metadata, Rootfs, VmManifest};
        use kameo::prelude::*;
        let vmid = ulid::Ulid::generate();
        let manifest = VmManifest {
            id: vmid,
            api_version: crate::manifest::MANIFEST_VERSION,
            observed: None,
            desired: DesiredState {
                metadata: Metadata {
                    name: "failed-start-fixture".into(),
                    ..Default::default()
                },
                compute: Compute {
                    vcpus: 1,
                    memory_bytes: 128 * 1024 * 1024,
                    ..Default::default()
                },
                rootfs: Some(Rootfs {
                    oci: "oci:/nonexistent-odorobo-fixture:latest".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        };
        let prepared = super::VMActor::prepare();
        let actor = prepared.actor_ref().clone();
        let handle = prepared.spawn((vmid, Some(manifest)));
        actor.ask(super::StartVM).await.unwrap();
        let mut info = None;
        for _ in 0..200 {
            let reply = actor
                .ask(crate::messages::vm::GetVMInfo { vmid: Some(vmid) })
                .await
                .unwrap();
            let observed = reply.observed.clone().unwrap();
            if observed.status != crate::manifest::ObservedStatus::Pending {
                info = Some((reply, observed));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let (reply, observed) = info.expect("image preparation must finish");
        assert_eq!(observed.status, crate::manifest::ObservedStatus::Error);
        let failure = observed.error.unwrap_or_default();
        assert!(
            failure.contains("skopeo"),
            "fixture must reach OCI acquisition: {failure}"
        );
        assert!(reply.config.is_some());
        assert!(
            actor
                .ask(crate::messages::vm::DeleteVM { vmid })
                .await
                .unwrap()
                .error
                .is_none()
        );
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn mismatched_identity_is_rejected_before_rootfs_acquisition() {
        use kameo::prelude::*;
        let manifest: crate::manifest::VmManifest = serde_json::from_str(include_str!(
            "../../../docs/fixtures/manifest/oci-rootfs.json"
        ))
        .unwrap();
        let prepared = super::VMActor::prepare();
        let handle = prepared.spawn((ulid::Ulid::generate(), Some(manifest)));
        let failure = handle
            .await
            .unwrap()
            .err()
            .expect("startup must reject mismatched identity");
        assert!(format!("{failure:?}").contains("identity"));
    }

    /// Full OCI rootfs boot through the actor: OCI root -> composefs image ->
    /// supervised virtiofsd -> CH direct kernel -> virtiofs root.
    /// Requires nested KVM, the host tools, and Zig for a static init stub, so
    /// it is ignored by default: `cargo test -p odorobo --bin odorobo oci_rootfs_vm_boots -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "boots a real VM; needs /dev/kvm, cloud-hypervisor, virtiofsd, skopeo, composefs host support, and zig"]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end test exercises one long scenario"
    )]
    async fn oci_rootfs_vm_boots_through_the_actor() {
        use crate::ch_driver::{actor::VMActor, containers};
        use crate::manifest::{
            Boot, Compute, DesiredState, MANIFEST_VERSION, Metadata, Placement, Rootfs, VmManifest,
        };
        use crate::messages::vm::{DeleteVM, GetConsoleHistory, GetWorkloadLogs};
        use kameo::prelude::*;
        use std::path::PathBuf;
        use std::time::Duration;
        use tracing::info;

        _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "odorobo=debug,mk_compressfs=debug".into()),
            )
            .with_test_writer()
            .try_init()
            .map_err(|err| info!(?err, "tracing subscriber already set"));

        let vmid = ulid::Ulid::generate();
        let fixture_dir = PathBuf::from("/var/lib/odorobo/vm-boot-fixtures").join(vmid.to_string());
        std::fs::create_dir_all(&fixture_dir).unwrap();
        let init_source = fixture_dir.join("init.c");
        let init_binary = fixture_dir.join("root-init");
        std::fs::write(&init_source, "#include <unistd.h>\nint main(void) { static const char s[] = \"odorobo-test-init\\n\"; write(1, s, sizeof(s)-1); for (;;) pause(); }\n").unwrap();
        let compiled = std::process::Command::new("zig")
            .args(["cc", "-target", "x86_64-linux-musl", "-static", "-O2", "-o"])
            .arg(&init_binary)
            .arg(&init_source)
            .output()
            .expect("zig is required for a static guest init fixture");
        assert!(
            compiled.status.success(),
            "zig init compile failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let oci_layout = crate::ch_driver::containers::test_support::OciFixture {
            init_binary: Some(&init_binary),
            ..Default::default()
        }
        .layout(&fixture_dir);
        let image_ref = format!("oci:{}:latest", oci_layout.display());
        let manifest = VmManifest {
            api_version: MANIFEST_VERSION,
            id: vmid,
            desired: DesiredState {
                metadata: Metadata {
                    name: "oci-boot-test".to_owned(),
                    ..Default::default()
                },
                compute: Compute {
                    vcpus: 1,
                    memory_bytes: 512 * 1024 * 1024,
                    ..Default::default()
                },
                storage: vec![],
                networks: vec![],
                placement: Placement::default(),
                boot: Boot {
                    start: true,
                    // The fixture image's OCI config sets Entrypoint to /bin/sh.
                    ..Default::default()
                },
                cloud_init: None,
                vsock: None,
                rootfs: Some(Rootfs {
                    oci: image_ref,
                    mode: crate::manifest::RootfsMode::Persistent,
                    stdin: false,
                    entrypoint: None,
                    cmd: None,
                }),
            },
            observed: None,
        };

        // prepare() gives access to the ActorRef before the actor runs.
        let prepared = VMActor::prepare();
        let actor_ref = prepared.actor_ref().clone();
        let handle = prepared.spawn((vmid, Some(manifest)));
        actor_ref.ask(super::StartVM).await.unwrap();

        // Require userspace execution, not merely a kernel driver log.
        let deadline = std::time::Instant::now()
            .checked_add(Duration::from_secs(120))
            .expect("deadline in range");
        let mut virtiofs_seen = false;
        let mut stopped = None;
        let mut last_console = Vec::new();
        let mut last_logs = Vec::new();
        while std::time::Instant::now() < deadline {
            let console = match actor_ref.ask(GetConsoleHistory { vmid }).await {
                Ok(reply) => reply,
                Err(err) => {
                    let termination = match handle.await {
                        Ok(Ok((_actor, reason))) => format!("actor stopped: {reason:?}"),
                        Ok(Err(start_error)) => format!("actor start failed: {start_error:?}"),
                        Err(join_error) => format!("actor task failed: {join_error}"),
                    };
                    panic!("console history ask failed: {err}; {termination}");
                }
            };
            last_console.clone_from(&console.history);
            // The fixture init prints its marker to stdout, which the split
            // console layout carries on the workload stream, not the serial one.
            let logs = match actor_ref.ask(GetWorkloadLogs { vmid }).await {
                Ok(reply) => reply,
                Err(err) => panic!(
                    "workload logs ask failed: {err}; console:\n{}",
                    String::from_utf8_lossy(&console.history)
                ),
            };
            last_logs.clone_from(&logs.logs);
            if String::from_utf8_lossy(&logs.logs).contains("odorobo-test-init") {
                virtiofs_seen = true;
                info!(
                    vmid = %vmid,
                    workload_tail = %String::from_utf8_lossy(&logs.logs
                        [logs.logs.len().saturating_sub(400)..]),
                    "guest workload console shows the fixture init"
                );
                break;
            }
            // A startup failure means the fixture init will never run; report
            // the console now instead of waiting out the boot deadline.
            if let Ok(Some(reason)) = actor_ref.ask(super::StartFailure).await {
                stopped = Some(reason);
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert!(
            virtiofs_seen,
            "guest never executed fixture init within 120s{}; console:\n{}\nworkload logs:\n{}\n(check that /var/lib/odorobo/microvm/initramfs.cpio.gz matches the crate: `just initramfs`)",
            stopped
                .as_ref()
                .map_or_else(String::new, |err| format!("; VM stopped: {err}")),
            String::from_utf8_lossy(&last_console),
            String::from_utf8_lossy(&last_logs)
        );

        // Cold stop must detach mounts but keep the lifecycle owner and upper
        // reachable. Deletion must not need the image or a new hypervisor.
        let persistent = PathBuf::from("/var/lib/odorobo/persistent-rootfs").join(vmid.to_string());
        std::fs::write(persistent.join("rootfs.upper/marker"), b"retained").unwrap();
        // Force a post-VMM-stop cleanup failure. A restart must not succeed
        // through a stale 'started' flag while the old root is still busy.
        let busy_mount = containers::rootfs_mount_path_for(&vmid.to_string());
        let mut busy = tokio::process::Command::new("sleep")
            .arg("100")
            .current_dir(&busy_mount)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let shutdown = actor_ref
            .ask(crate::messages::vm::ShutdownVM { vmid })
            .await;
        assert!(
            shutdown.is_err(),
            "shutdown must retain the busy root: {shutdown:?}"
        );
        let start = actor_ref.ask(super::StartVM).await;
        assert!(
            start.is_err(),
            "restart must retry retained teardown: {start:?}"
        );
        busy.kill().await.unwrap();
        actor_ref
            .ask(super::StartVM)
            .await
            .expect("restart after unblocking teardown");
        let mut restarted = false;
        for _ in 0..120 {
            if actor_ref.ask(super::CheckRunning).await.is_ok() {
                restarted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(restarted, "restart must finish image preparation and boot");
        // Deliver a predecessor's queued exit only after replacement startup.
        actor_ref
            .ask(super::HypervisorExited { generation: 1 })
            .await
            .unwrap();
        actor_ref.ask(super::CheckRunning).await.unwrap();
        let busy_mount = containers::rootfs_mount_path_for(&vmid.to_string());
        let mut busy = tokio::process::Command::new("sleep")
            .arg("100")
            .current_dir(&busy_mount)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        actor_ref.ask(super::CrashHypervisor).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            actor_ref
                .ask(crate::messages::vm::GetVMInfo { vmid: Some(vmid) })
                .await
                .unwrap()
                .config
                .is_some()
        );
        assert!(
            actor_ref
                .ask(crate::messages::vm::DeleteVM { vmid })
                .await
                .unwrap()
                .error
                .is_some(),
            "crash must retain busy-root cleanup owner"
        );
        busy.kill().await.unwrap();
        actor_ref
            .ask(crate::messages::vm::ShutdownVM { vmid })
            .await
            .expect("cold shutdown");
        assert!(persistent.join("rootfs.upper/marker").exists());
        assert!(
            actor_ref
                .ask(crate::messages::vm::GetVMInfo { vmid: Some(vmid) })
                .await
                .unwrap()
                .config
                .is_some()
        );
        let deleted = actor_ref
            .ask(DeleteVM { vmid })
            .await
            .expect("DeleteVM reply");
        assert_eq!(
            deleted.error, None,
            "actor rootfs teardown must report clean deletion"
        );
        assert!(
            !persistent.exists(),
            "DeleteVM must remove stopped persistent state"
        );
        // The actor handle resolves once on_stop teardown has finished.
        let (_, reason) = handle
            .await
            .expect("actor task")
            .expect("actor stopped cleanly");
        info!(?reason, "oci boot test actor stopped");
        tokio::time::sleep(Duration::from_secs(1)).await;

        let socket = containers::socket_path_for(&vmid.to_string());
        let mount = containers::rootfs_mount_path_for(&vmid.to_string());
        assert!(!socket.exists(), "virtiofsd socket leaked after stop");
        assert!(!mount.exists(), "rootfs mount point leaked after stop");
        let leaked = std::process::Command::new("pgrep")
            .arg("virtiofsd")
            .output()
            .expect("pgrep");
        assert!(
            !leaked.status.success(),
            "virtiofsd process leaked after actor stop"
        );
        std::fs::remove_dir_all(fixture_dir).unwrap();
    }

    /// Synopsis of a fixture image whose init probes writes: it prints
    /// `odorobo-write-probe root=<ok|fail> tmp=<ok|fail>` and then parks, so a
    /// boot test can tell where writes landed.
    fn write_probe_fixture(
        vmid: ulid::Ulid,
        mode: crate::manifest::RootfsMode,
    ) -> (std::path::PathBuf, crate::manifest::VmManifest) {
        use crate::manifest::{
            Boot, Compute, DesiredState, MANIFEST_VERSION, Metadata, Placement, Rootfs, VmManifest,
        };
        let fixture_dir =
            std::path::PathBuf::from("/var/lib/odorobo/vm-boot-fixtures").join(vmid.to_string());
        std::fs::create_dir_all(&fixture_dir).unwrap();
        let init_source = fixture_dir.join("init.c");
        let init_binary = fixture_dir.join("root-init");
        std::fs::write(
            &init_source,
            "#include <fcntl.h>\n#include <unistd.h>\n\
             static int probe(const char *path) { int fd = open(path, O_CREAT|O_WRONLY|O_TRUNC, 0644); if (fd < 0) return 0; write(fd, \"x\", 1); close(fd); return 1; }\n\
             int main(void) { static const char s[] = \"odorobo-write-probe root=\"; write(1, s, sizeof(s)-1); write(1, probe(\"/write-proof\") ? \"ok\" : \"fail\", 4); static const char t[] = \" tmp=\"; write(1, t, sizeof(t)-1); write(1, probe(\"/tmp/write-proof\") ? \"ok\" : \"fail\", 4); write(1, \"\\n\", 1); for (;;) pause(); }\n",
        )
        .unwrap();
        let compiled = std::process::Command::new("zig")
            .args(["cc", "-target", "x86_64-linux-musl", "-static", "-O2", "-o"])
            .arg(&init_binary)
            .arg(&init_source)
            .output()
            .expect("zig is required for a static guest init fixture");
        assert!(
            compiled.status.success(),
            "zig init compile failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let oci_layout = crate::ch_driver::containers::test_support::OciFixture {
            init_binary: Some(&init_binary),
            ..Default::default()
        }
        .layout(&fixture_dir);
        let manifest = VmManifest {
            api_version: MANIFEST_VERSION,
            id: vmid,
            desired: DesiredState {
                metadata: Metadata {
                    name: "oci-write-probe".to_owned(),
                    ..Default::default()
                },
                compute: Compute {
                    vcpus: 1,
                    memory_bytes: 512 * 1024 * 1024,
                    ..Default::default()
                },
                storage: vec![],
                networks: vec![],
                placement: Placement::default(),
                boot: Boot {
                    start: true,
                    ..Default::default()
                },
                cloud_init: None,
                vsock: None,
                rootfs: Some(Rootfs {
                    oci: format!("oci:{}:latest", oci_layout.display()),
                    mode,
                    stdin: false,
                    entrypoint: None,
                    cmd: None,
                }),
            },
            observed: None,
        };
        (fixture_dir, manifest)
    }

    /// Full ephemeral OCI boot through the actor: the host serves the composefs
    /// image read-only and the guest init builds the root overlay from its own
    /// tmpfs, so the fixture init's write to `/` must succeed there.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "boots a real VM; needs /dev/kvm, cloud-hypervisor, virtiofsd, skopeo, composefs host support, and zig"]
    async fn oci_ephemeral_vm_writes_into_its_guest_tmpfs() {
        use crate::ch_driver::VMInstance;
        use crate::ch_driver::{actor::VMActor, containers};
        use crate::manifest::RootfsMode;
        use crate::messages::vm::{DeleteVM, GetConsoleHistory, GetWorkloadLogs};
        use kameo::prelude::*;
        use std::time::Duration;
        use tracing::info;

        _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "odorobo=debug,mk_compressfs=debug".into()),
            )
            .with_test_writer()
            .try_init()
            .map_err(|err| info!(?err, "tracing subscriber already set"));

        let vmid = ulid::Ulid::generate();
        let (fixture_dir, manifest) = write_probe_fixture(vmid, RootfsMode::Ephemeral);

        let prepared = VMActor::prepare();
        let actor_ref = prepared.actor_ref().clone();
        let handle = prepared.spawn((vmid, Some(manifest)));
        actor_ref.ask(super::StartVM).await.unwrap();

        let deadline = std::time::Instant::now()
            .checked_add(Duration::from_secs(120))
            .expect("deadline in range");
        let mut last_console = Vec::new();
        let mut last_logs = Vec::new();
        let mut probe_seen = false;
        while std::time::Instant::now() < deadline {
            let console = actor_ref
                .ask(GetConsoleHistory { vmid })
                .await
                .expect("console history ask");
            last_console.clone_from(&console.history);
            let logs = actor_ref
                .ask(GetWorkloadLogs { vmid })
                .await
                .expect("workload logs ask");
            last_logs.clone_from(&logs.logs);
            if String::from_utf8_lossy(&logs.logs).contains("root=") {
                probe_seen = true;
                break;
            }
            // A startup failure means the fixture init will never run.
            if let Ok(Some(reason)) = actor_ref.ask(super::StartFailure).await {
                panic!(
                    "VM never started: {reason}; console:\n{}\nworkload logs:\n{}",
                    String::from_utf8_lossy(&last_console),
                    String::from_utf8_lossy(&last_logs)
                );
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert!(
            probe_seen,
            "the fixture init never ran within 120s; console:\n{}\nworkload logs:\n{}\n(check that /var/lib/odorobo/microvm/initramfs.cpio.gz matches the crate: `just initramfs`)",
            String::from_utf8_lossy(&last_console),
            String::from_utf8_lossy(&last_logs)
        );
        let probe = String::from_utf8_lossy(&last_logs).into_owned();
        assert!(
            probe.contains("root=ok") && probe.contains("tmp=ok"),
            "an ephemeral root must accept writes through its tmpfs overlay: {probe}"
        );

        // The host side stays read-only: writes only ever land in the guest.
        let mount = containers::rootfs_mount_path_for(&vmid.to_string());
        assert!(
            std::fs::write(mount.join("host-write"), b"x").is_err(),
            "ephemeral host mount must reject writes"
        );
        let state = VMInstance::runtime_dir_for(&vmid.to_string()).join("rootfs-state");
        assert!(
            !state.exists(),
            "an ephemeral root must keep no host-side upper"
        );

        let deleted = actor_ref
            .ask(DeleteVM { vmid })
            .await
            .expect("DeleteVM reply");
        assert_eq!(deleted.error, None, "ephemeral VM must delete cleanly");
        handle.await.unwrap().unwrap();
        std::fs::remove_dir_all(fixture_dir).unwrap();
    }

    /// Full read-only OCI boot through the actor: the guest switches into the
    /// served root with `pivot_root`, parking the old tree on its /tmp scratch
    /// mount, so the root write must fail while the scratch write succeeds.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "boots a real VM; needs /dev/kvm, cloud-hypervisor, virtiofsd, skopeo, composefs host support, and zig"]
    async fn oci_readonly_vm_keeps_writes_out_of_the_image() {
        use crate::ch_driver::actor::VMActor;
        use crate::manifest::RootfsMode;
        use crate::messages::vm::{DeleteVM, GetConsoleHistory, GetWorkloadLogs};
        use kameo::prelude::*;
        use std::time::Duration;

        let vmid = ulid::Ulid::generate();
        let (fixture_dir, manifest) = write_probe_fixture(vmid, RootfsMode::ReadOnly);
        let prepared = VMActor::prepare();
        let actor_ref = prepared.actor_ref().clone();
        let handle = prepared.spawn((vmid, Some(manifest)));
        actor_ref.ask(super::StartVM).await.unwrap();

        let deadline = std::time::Instant::now()
            .checked_add(Duration::from_secs(120))
            .expect("deadline in range");
        let mut last_console = Vec::new();
        let mut last_logs = Vec::new();
        let mut probe_seen = false;
        while std::time::Instant::now() < deadline {
            let console = actor_ref
                .ask(GetConsoleHistory { vmid })
                .await
                .expect("console history ask");
            last_console.clone_from(&console.history);
            let logs = actor_ref
                .ask(GetWorkloadLogs { vmid })
                .await
                .expect("workload logs ask");
            last_logs.clone_from(&logs.logs);
            if String::from_utf8_lossy(&logs.logs).contains("root=") {
                probe_seen = true;
                break;
            }
            if let Ok(Some(reason)) = actor_ref.ask(super::StartFailure).await {
                panic!(
                    "VM never started: {reason}; console:\n{}\nworkload logs:\n{}",
                    String::from_utf8_lossy(&last_console),
                    String::from_utf8_lossy(&last_logs)
                );
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert!(
            probe_seen,
            "the fixture init never ran within 120s; console:\n{}\nworkload logs:\n{}\n(check that /var/lib/odorobo/microvm/initramfs.cpio.gz matches the crate: `just initramfs`)",
            String::from_utf8_lossy(&last_console),
            String::from_utf8_lossy(&last_logs)
        );
        let probe = String::from_utf8_lossy(&last_logs).into_owned();
        assert!(
            probe.contains("root=fail"),
            "a read-only root must reject writes: {probe}"
        );
        assert!(
            probe.contains("tmp=ok"),
            "the scratch tmpfs must stay writable after the pivot: {probe}"
        );

        let deleted = actor_ref
            .ask(DeleteVM { vmid })
            .await
            .expect("DeleteVM reply");
        assert_eq!(deleted.error, None, "read-only VM must delete cleanly");
        handle.await.unwrap().unwrap();
        std::fs::remove_dir_all(fixture_dir).unwrap();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationFinished;

#[derive(RemoteActor)]
pub struct VMActor {
    pub vmid: ulid::Ulid,
    /// path to the Cloud Hypervisor socket, in /run/odorobo/vms/<VMID>/ch.sock
    pub vm_instance: VMInstance,
    pub migration_state: Option<MigrationState>,
    /// Kernel and init output from the serial console.
    pub console: Console,
    /// Workload stdin/stdout/stderr from the virtio console.
    pub workload_console: Console,
    /// Supervised virtiofsd child (extra process supervised by this actor)
    /// when the VM boots an OCI rootfs over virtiofs (issue #112).
    pub virtiofs: Option<VirtioFsSupervisor>,
    /// This VM's digest-pinned composefs mount of its OCI rootfs.
    pub rootfs_mount: Option<PreparedRootfs>,
    /// Desired provider-neutral intent retained for VM info and migration.
    /// The translated Cloud Hypervisor config lives only in `VMInstance`.
    pub manifest: Option<VmManifest>,
    /// Manifest digest of the OCI image this VM's rootfs was last prepared
    /// from. Repository GC keeps that image alive until the actor is deleted.
    rootfs_digest: Option<String>,
    /// OCI lifecycle owners survive failed startup and cold stop, so cleanup
    /// remains reachable without pulling or booting the image again.
    startup_error: Option<String>,
    /// A startup failure from `on_start` not yet returned to a `StartVM`
    /// caller. The first `StartVM` after spawn reports it instead of
    /// immediately re-running the whole (possibly minutes-long) start.
    unreported_start_failure: Option<String>,
    started: bool,
    /// Attempt number of an OCI image preparation still running off the mailbox.
    starting: Option<u64>,
    start_attempts: u64,
    /// Distinguishes queued exit notifications from previous VMM instances.
    generation: u64,
}

#[cfg(test)]
struct CrashHypervisor;

#[cfg(test)]
impl Message<CrashHypervisor> for VMActor {
    type Reply = ();
    async fn handle(&mut self, _msg: CrashHypervisor, _ctx: &mut Context<Self, Self::Reply>) {
        self.vm_instance.kill_hypervisor_for_test().await;
    }
}

#[cfg(test)]
struct StartFailure;

/// A startup failure means the VM will never reach the guest init; waiting
/// for the boot deadline after one would hide the console evidence.
#[cfg(test)]
impl Message<StartFailure> for VMActor {
    type Reply = Option<String>;
    async fn handle(
        &mut self,
        _msg: StartFailure,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.startup_error.clone()
    }
}

#[cfg(test)]
struct CheckRunning;

#[cfg(test)]
impl Message<CheckRunning> for VMActor {
    type Reply = Result<(), String>;
    async fn handle(
        &mut self,
        _msg: CheckRunning,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if !self.started {
            return Err("not started".into());
        }
        self.vm_instance
            .ping()
            .await
            .map(|_| ())
            .map_err(|err| err.to_string())
    }
}

struct HypervisorExited {
    generation: u64,
}

impl Message<HypervisorExited> for VMActor {
    type Reply = ();
    async fn handle(&mut self, msg: HypervisorExited, ctx: &mut Context<Self, Self::Reply>) {
        if msg.generation != self.generation {
            return;
        }
        self.started = false;
        if self
            .manifest
            .as_ref()
            .is_some_and(|m| m.desired.rootfs.is_some())
        {
            let cleanup = async {
                self.vm_instance.stop().await?;
                teardown_rootfs(&mut self.virtiofs, &mut self.rootfs_mount).await
            }
            .await;
            self.startup_error = Some(format!("hypervisor exited; cleanup: {cleanup:?}"));
            warn!(vmid=%self.vmid, ?cleanup, "retaining OCI owner after hypervisor exit");
        } else {
            _ = ctx.actor_ref().stop_gracefully().await;
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct StartVM;

/// Manifest digest of the OCI image this VM's rootfs was last prepared from,
/// if any. Repository GC pins that image as long as this actor exists.
#[derive(Serialize, Deserialize, Debug)]
pub struct GetRootfsDigest;

#[remote_message]
impl Message<GetRootfsDigest> for VMActor {
    type Reply = Option<String>;

    async fn handle(
        &mut self,
        _msg: GetRootfsDigest,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.rootfs_digest.clone()
    }
}

#[remote_message]
impl Message<StartVM> for VMActor {
    type Reply = Result<(), String>;

    /// For OCI rootfs VMs this only starts image preparation; progress and
    /// failures are reported through `GetVMInfo`'s observed state.
    async fn handle(&mut self, _msg: StartVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        if self.started || self.starting.is_some() {
            return Ok(());
        }
        if let Some(message) = self.unreported_start_failure.take() {
            return Err(message);
        }
        if !self.has_rootfs() {
            if let Err(err) = self.start(ctx.actor_ref().clone(), None).await {
                return Err(self.rollback_start(&err).await);
            }
            return Ok(());
        }
        // Release any retained mounts now, so a blocked teardown fails this call instead of the boot.
        let teardown = async {
            self.vm_instance.stop().await?;
            teardown_rootfs(&mut self.virtiofs, &mut self.rootfs_mount).await?;
            self.vm_instance.purge_instance_data()
        }
        .await;
        if let Err(err) = teardown {
            let message = format!("retained rootfs teardown failed: {err:#}");
            self.startup_error = Some(message.clone());
            return Err(message);
        }
        let Some(rootfs) = self
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.desired.rootfs.clone())
        else {
            return Err("rootfs VM has no rootfs intent".into());
        };
        self.start_attempts = self.start_attempts.saturating_add(1);
        let attempt = self.start_attempts;
        self.starting = Some(attempt);
        self.startup_error = None;
        let actor_ref = ctx.actor_ref().clone();
        let vmid = self.vmid;
        tokio::spawn(async move {
            let image = crate::ch_driver::containers::prepare_image(&rootfs).await;
            if actor_ref.tell(ImageReady { attempt, image }).await.is_err() {
                debug!(%vmid, "VM actor stopped before its image was ready");
            }
        });
        Ok(())
    }
}

struct ImageReady {
    attempt: u64,
    image: Result<crate::ch_driver::containers::PreparedImage>,
}

impl Message<ImageReady> for VMActor {
    type Reply = ();

    async fn handle(&mut self, msg: ImageReady, ctx: &mut Context<Self, Self::Reply>) {
        // Shutdown, delete or a newer start cleared or replaced this attempt.
        if self.starting != Some(msg.attempt) {
            return;
        }
        self.starting = None;
        let result = match msg.image {
            Ok(image) => self.start(ctx.actor_ref().clone(), Some(image)).await,
            Err(err) => Err(err.wrap_err("prepare OCI image")),
        };
        if let Err(err) = result {
            let message = self.rollback_start(&err).await;
            error!(vmid = %self.vmid, %message, "VM startup failed; retaining lifecycle owner for retry or delete");
        }
    }
}

/// One dependency-ordered cleanup path for startup rollback, stop and delete.
/// Keep the prepared state on failure so the same operation can be retried.
async fn teardown_rootfs(
    virtiofs: &mut Option<VirtioFsSupervisor>,
    rootfs: &mut Option<PreparedRootfs>,
) -> Result<()> {
    if let Some(supervisor) = virtiofs.as_ref() {
        supervisor.stop().await;
    }
    *virtiofs = None;
    if let Some(prepared) = rootfs.as_ref() {
        crate::ch_driver::containers::unmount_rootfs(prepared).await?;
    }
    *rootfs = None;
    Ok(())
}

/// Asks the network actor to publish this VM's ports.
///
/// The actor resolves the guest address from the DHCP lease, so this only
/// queues the mapping; the rule appears once the guest has a lease.
async fn publish_ports(vmid: ulid::Ulid, manifest: &VmManifest) {
    if manifest.desired.rootfs.is_none() {
        return;
    }
    let actor = match ActorRef::<NetworkAgentActor>::lookup("network_actor").await {
        Ok(Some(actor)) => actor,
        Ok(None) => {
            warn!(%vmid, "network actor is not running; published ports were not created");
            return;
        }
        Err(err) => {
            warn!(%vmid, ?err, "failed to look up network actor; published ports were not created");
            return;
        }
    };
    for network in &manifest.desired.networks {
        if network.ports.is_empty() {
            continue;
        }
        let Some(mac_address) = network.mac_address.as_deref() else {
            continue;
        };
        if let Err(err) = actor
            .ask(PublishPorts {
                vmid,
                mac_address: mac_address.to_owned(),
                ports: network.ports.clone(),
            })
            .await
        {
            warn!(%vmid, network = %network.id, ?err, "failed to request port publishing");
        }
    }
}

impl VMActor {
    fn has_rootfs(&self) -> bool {
        self.manifest
            .as_ref()
            .is_some_and(|manifest| manifest.desired.rootfs.is_some())
    }

    /// Stop the VMM and release its rootfs; the error message is stored for `GetVMInfo`.
    async fn rollback_start(&mut self, err: &stable_eyre::Report) -> String {
        let cleanup = async {
            self.vm_instance.stop().await?;
            teardown_rootfs(&mut self.virtiofs, &mut self.rootfs_mount).await?;
            self.vm_instance.purge_instance_data()
        }
        .await;
        let message = format!("{err:#}; cleanup: {cleanup:?}");
        self.startup_error = Some(message.clone());
        message
    }

    /// Prepare the rootfs mount and start the supervised virtiofsd child
    /// before Cloud Hypervisor creates the VM; a failure rolls back all of it.
    async fn start_rootfs(
        &mut self,
        vmid: ulid::Ulid,
        rootfs: &crate::manifest::Rootfs,
        image: Option<crate::ch_driver::containers::PreparedImage>,
    ) -> Result<()> {
        let virtiofs = &mut self.virtiofs;
        let rootfs_mount = &mut self.rootfs_mount;
        let runtime_dir = VMInstance::runtime_dir_for(&vmid.to_string());
        // Resolve everything that can fail cheaply BEFORE mounting, so
        // early errors cannot leak a mounted rootfs.
        let program = crate::ch_driver::containers::virtiofsd_path().ok_or_else(|| {
            eyre!("virtiofsd not found (looked in PATH and /usr/libexec) — `dnf install virtiofsd`")
        })?;
        let image = image.ok_or_else(|| eyre!("OCI image was not prepared before start"))?;
        // Record before mounting: a retained VM keeps its image root on failure.
        self.rootfs_digest = Some(image.digest().to_owned());
        crate::ch_driver::containers::mount_rootfs_owned(
            &vmid.to_string(),
            rootfs,
            &image,
            rootfs_mount,
        )
        .await?;
        // Release the GC guard once the mount can hold the image itself.
        drop(image);
        // An ephemeral root's writable layer is a guest-side tmpfs, so its
        // share is served read-only; a persistent root keeps a host upper.
        let rootfs_spec = VirtiofsdSpec {
            program,
            socket: crate::ch_driver::containers::socket_path_for(&vmid.to_string()),
            shared_dir: crate::ch_driver::containers::rootfs_mount_path_for(&vmid.to_string()),
            log: runtime_dir.join("virtiofsd.log"),
            read_only: rootfs.mode == crate::manifest::RootfsMode::Ephemeral,
        };
        let start = async {
            *virtiofs = Some(VirtioFsSupervisor::start(rootfs_spec));
            virtiofs
                .as_ref()
                .unwrap()
                .wait_socket_ready(std::time::Duration::from_secs(15))
                .await
        }
        .await;
        if let Err(err) = start {
            let cleanup = teardown_rootfs(virtiofs, rootfs_mount).await;
            return Err(err.wrap_err(format!("virtiofs startup rollback: {cleanup:?}")));
        }
        info!(%vmid, "virtiofsd supervised child started and socket ready");
        Ok(())
    }

    /// `image` must be set for rootfs VMs; `prepare_image` builds it off the mailbox.
    async fn start(
        &mut self,
        actor_ref: ActorRef<Self>,
        image: Option<crate::ch_driver::containers::PreparedImage>,
    ) -> Result<()> {
        // Clean any previous failed acquisition before trying again. Keep its
        // ownership inventory if teardown fails; never overwrite live mounts.
        self.vm_instance.stop().await?;
        teardown_rootfs(&mut self.virtiofs, &mut self.rootfs_mount).await?;
        self.vm_instance.purge_instance_data()?;
        let vmid = self.vmid;
        let vm_config = self.manifest.clone();
        // Boot is manifest intent, not a Cloud Hypervisor default. Preserve it
        // separately because VMInstance also supports create-without-boot paths.
        let boot = vm_config
            .as_ref()
            .is_some_and(|manifest| manifest.desired.boot.start);
        let vm_config_for_ch = vm_config.as_ref().map(to_vm_config).transpose()?;

        // OCI rootfs (issue #112): the composefs mount and the supervised
        // virtiofsd processes must exist BEFORE Cloud Hypervisor creates the
        // VM; the fs devices connect at create time.
        if let Some(manifest) = &vm_config
            && let Some(rootfs) = &manifest.desired.rootfs
        {
            self.start_rootfs(vmid, rootfs, image).await?;
        }

        let virtiofs = &mut self.virtiofs;
        let rootfs_mount = &mut self.rootfs_mount;

        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| eyre!("hypervisor generation overflow"))?;
        // An OCI rootfs boots our own kernel and initramfs, so the workload's
        // stdio can ride the virtio console while kernel and init logs stay on
        // the serial console; a disk VM boots its own kernel and keeps only the
        // serial console its cmdline expects.
        let split_console = vm_config
            .as_ref()
            .is_some_and(|manifest| manifest.desired.rootfs.is_some());
        let transformer = split_console
            .then(|| TransformChain::with_console(ConsoleTransform::split_workload_console()));
        // Create without booting: the console spool must be attached before any
        // guest output, because a virtio console is drained into the socket and
        // drops whatever the guest writes before a client connects.
        VMInstance::spawn_into(
            &mut self.vm_instance,
            &vmid.to_string(),
            vm_config_for_ch,
            false,
            transformer,
        )
        .await?;

        let console = Console::default();
        let workload_console = Console::default();
        // A migration receiver has no config yet; its serial socket is created
        // only when the migrated VM is restored.
        if vm_config.is_some() {
            let system_socket = self.vm_instance.console_socket_path();
            let workload_socket =
                split_console.then(|| self.vm_instance.workload_console_socket_path());
            let attach_result = async {
                console.attach_socket(system_socket).await?;
                if let Some(socket) = workload_socket {
                    workload_console.attach_socket(socket).await?;
                }
                Ok::<(), Report>(())
            }
            .await;
            if let Err(err) = attach_result {
                // Stop CH before removing a root from a potentially live guest.
                let cleanup = async {
                    self.vm_instance.stop().await?;
                    teardown_rootfs(virtiofs, rootfs_mount).await?;
                    self.vm_instance.purge_instance_data()
                }
                .await;
                return Err(err.wrap_err(format!("console startup rollback: {cleanup:?}")));
            }
        }

        if boot && let Some(manifest) = &vm_config {
            self.vm_instance.boot().await?;
            publish_ports(vmid, manifest).await;
        }

        let generation = self.generation;
        let child_exit = self.vm_instance.watch_child();
        tokio::spawn(async move {
            match child_exit.await {
                Ok(None) => {} // Normal teardown took and reaped the child.
                other => {
                    error!(%vmid, ?other, "hypervisor exited outside teardown");
                    _ = actor_ref.tell(HypervisorExited { generation }).await;
                }
            }
        });

        self.console = console;
        self.workload_console = workload_console;
        self.started = true;
        self.startup_error = None;
        Ok(())
    }
}

impl Actor for VMActor {
    type Args = (ulid::Ulid, Option<VmManifest>);
    type Error = Report;

    async fn on_start((vmid, manifest): Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        if manifest
            .as_ref()
            .is_some_and(|manifest| manifest.id != vmid)
        {
            return Err(eyre!("VM message identity does not match manifest ID"));
        }
        let mut actor = Self {
            vmid,
            vm_instance: VMInstance::stopped(&vmid.to_string()),
            migration_state: None,
            console: Console::default(),
            workload_console: Console::default(),
            virtiofs: None,
            rootfs_mount: None,
            manifest,
            rootfs_digest: None,
            startup_error: None,
            unreported_start_failure: None,
            started: false,
            generation: 0,
            starting: None,
            start_attempts: 0,
        };
        // OCI rootfs VMs wait for StartVM, which prepares the image off the mailbox.
        if actor.has_rootfs() {
            return Ok(actor);
        }
        if let Err(err) = actor.start(actor_ref, None).await {
            let message = actor.rollback_start(&err).await;
            error!(%vmid, %message, "VM startup failed; retaining lifecycle owner for retry or delete");
            actor.unreported_start_failure = Some(message);
        }
        Ok(actor)
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> std::result::Result<(), Self::Error> {
        match reason {
            ActorStopReason::Normal => {
                info!(vmid = %self.vmid, "stopping VM instance");
            }
            ActorStopReason::Killed => {
                error!(vmid = %self.vmid, "VM killed");
            }
            ActorStopReason::Panicked(err) => {
                error!(vmid = %self.vmid, ?err, "VM panicked");
            }
            _ => {
                warn!(vmid = %self.vmid, "unknown stop reason");
            }
        }

        // Never purge the runtime tree until every rootfs mount is detached.
        self.vm_instance.stop().await?;
        teardown_rootfs(&mut self.virtiofs, &mut self.rootfs_mount).await?;
        self.vm_instance.purge_instance_data()
    }
}

// allow conversion from VMActor to VMInstance to call API
impl From<VMActor> for VMInstance {
    fn from(actor: VMActor) -> Self {
        actor.vm_instance
    }
}

#[remote_message]
impl Message<GetConsoleHistory> for VMActor {
    type Reply = GetConsoleHistoryReply;

    async fn handle(
        &mut self,
        _msg: GetConsoleHistory,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        GetConsoleHistoryReply {
            history: self.console.history().await,
        }
    }
}

#[remote_message]
impl Message<GetWorkloadLogs> for VMActor {
    type Reply = GetWorkloadLogsReply;

    async fn handle(
        &mut self,
        _msg: GetWorkloadLogs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        GetWorkloadLogsReply {
            logs: self.workload_console.history().await,
        }
    }
}

#[remote_message]
impl Message<SendConsoleInput> for VMActor {
    type Reply = SendConsoleInputReply;

    async fn handle(
        &mut self,
        msg: SendConsoleInput,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let written = msg.input.len();
        // The workload owns the virtio console's stdin; only fall back to the
        // serial console when no workload console is attached.
        let target = if self.workload_console.is_attached().await {
            &self.workload_console
        } else {
            &self.console
        };
        match target.write_input(&msg.input).await {
            Ok(()) => SendConsoleInputReply {
                written,
                error: None,
            },
            Err(err) => {
                error!(vmid = %self.vmid, ?err, "failed to write to console");
                SendConsoleInputReply {
                    written: 0,
                    error: Some(err.to_string()),
                }
            }
        }
    }
}

#[remote_message]
impl Message<GetVMInfo> for VMActor {
    type Reply = GetVMInfoReply;
    async fn handle(
        &mut self,
        _msg: GetVMInfo,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use crate::manifest::{ObservedState, ObservedStatus};
        let status = if self.started {
            ObservedStatus::Running
        } else if self.starting.is_some() {
            ObservedStatus::Pending
        } else if self.startup_error.is_some() {
            ObservedStatus::Error
        } else {
            ObservedStatus::Stopped
        };
        GetVMInfoReply {
            vmid: self.vmid,
            config: self.manifest.clone(),
            observed: Some(ObservedState {
                status,
                error: self.startup_error.clone(),
                ..Default::default()
            }),
        }
    }
}

#[remote_message]
#[allow(clippy::unused_async_trait_impl)]
impl Message<GetVMHeartbeat> for VMActor {
    type Reply = GetVMHeartbeatReply;

    async fn handle(
        &mut self,
        _msg: GetVMHeartbeat,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        GetVMHeartbeatReply { vmid: self.vmid }
    }
}

#[remote_message]
impl Message<MigrateVMReceive> for VMActor {
    type Reply = MigrateVMReceiveReply;

    async fn handle(
        &mut self,
        msg: MigrateVMReceive,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // if there's a task already ongoing
        if let Some(migration_state) = &self.migration_state {
            return MigrateVMReceiveReply {
                listening_address: migration_state.listening_address.clone(),
                error: None,
            };
        }

        // OCI rootfs VMs are not migratable: the virtiofs device is
        // vhost-user, whose device state cannot be serialized by CH's
        // migration stream (issue #112). Reschedule cold instead — the
        // destination can mount the same digest-pinned rootfs exactly.
        if msg.config.desired.rootfs.is_some() {
            return MigrateVMReceiveReply {
                listening_address: String::new(),
                error: Some(
                    "VM has an OCI rootfs (virtiofs/composefs); live migration is not supported for vhost-user devices — reschedule the VM cold instead"
                        .to_owned(),
                ),
            };
        }

        let prep_config = msg.config.clone();

        // Translate before opening a receive socket so invalid intent cannot
        // leave behind a migration listener that can never complete.
        let config = match to_vm_config(&msg.config) {
            Ok(config) => config,
            Err(error) => {
                return MigrateVMReceiveReply {
                    listening_address: String::new(),
                    error: Some(error.to_string()),
                };
            }
        };

        // Start receiving migration on the destination VM (this actor).
        let (listening_address, migration_task) = match self.vm_instance.receive_migration().await {
            Ok(result) => result,
            Err(error) => {
                return MigrateVMReceiveReply {
                    listening_address: String::new(),
                    error: Some(error.to_string()),
                };
            }
        };

        self.migration_state = Some(MigrationState {
            migration_task: Some(migration_task),
            listening_address: listening_address.clone(),
            config,
        });

        let console = self.console.clone();
        let console_socket_path = self.vm_instance.console_socket_path();
        tokio::spawn(async move {
            // Bound the wait: a socket that never appears must not keep a
            // task (and Console clone) alive forever.
            for attempt in 1..=120 {
                match console.attach_socket(console_socket_path.clone()).await {
                    Ok(()) => return,
                    Err(err) => {
                        trace!(
                            ?err,
                            attempt, "serial console socket not ready during migration"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
            warn!(
                "serial console socket never became ready after migration; giving up on the console spool"
            );
        });

        let actor_ref = ctx.actor_ref().clone();

        let vmid = self.vmid;

        // now spawn a task for itself
        // to actually prep the migration while we're receiving the migration stream
        tokio::spawn(async move {
            if let Err(err) = actor_ref
                .tell(PrepMigration {
                    vmid,
                    config: prep_config,
                })
                .await
            {
                error!(
                    ?err,
                    "failed to start migration prep on destination VM actor"
                );
            }
        });

        // send migration finished notification in a separate task, after the prep is done
        if let Some(migration_state) = self.migration_state.as_mut() {
            // take the task value out and await that
            if let Some(migration_task) = migration_state.migration_task.take() {
                // NOTE: this is kinda scuffed
                let actor_ref = ctx.actor_ref().clone();
                tokio::spawn(async move {
                    if let Err(err) = migration_task.await {
                        error!(?err, "migration task join failed");
                    }

                    if let Err(err) = actor_ref.tell(MigrationFinished).await {
                        error!(?err, "failed to notify actor that migration finished");
                    }
                });
            }
        }

        MigrateVMReceiveReply {
            listening_address,
            error: None,
        }
    }
}

#[remote_message]
#[allow(clippy::unused_async_trait_impl)]
impl Message<MigrationFinished> for VMActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: MigrationFinished,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if self.migration_state.take().is_some() {
            info!(vmid = %self.vmid, "migration finished, cleared migration state");
        } else {
            warn!(vmid = %self.vmid, "received migration finished notification with no active migration state");
        }
    }
}

#[remote_message]
impl Message<PrepMigration> for VMActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: PrepMigration,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        info!(vmid = %self.vmid, "PrepMigration handler invoked");
        // Preparation is best-effort here because the remote receive operation
        // has no fallible reply channel; report failures and let migration state
        // cleanup handle the failed attempt.
        if msg.config.desired.rootfs.is_some() {
            error!(
                vmid = %self.vmid,
                "refusing to prep migration: OCI rootfs (virtiofs/composefs) VMs are not migratable"
            );
            return;
        }
        let config = match to_vm_config(&msg.config) {
            Ok(config) => config,
            Err(error) => {
                error!(?error, "failed to convert migration manifest");
                return;
            }
        };
        if let Err(error) = self.vm_instance.prep_config(config).await {
            error!(?error, "failed to prepare migrated VM configuration");
        }
    }
}

#[remote_message]
impl Message<ShutdownVM> for VMActor {
    type Reply = Result<(), String>;
    async fn handle(
        &mut self,
        _msg: ShutdownVM,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.starting = None;
        self.vm_instance
            .stop()
            .await
            .map_err(|err| format!("{err:#}"))?;
        self.started = false;
        teardown_rootfs(&mut self.virtiofs, &mut self.rootfs_mount)
            .await
            .map_err(|err| format!("{err:#}"))?;
        self.vm_instance
            .purge_instance_data()
            .map_err(|err| format!("{err:#}"))?;
        self.started = false;
        // Keep OCI ownership and identity reachable for restart/delete. This
        // is process-local lifecycle state, not durable scheduler inventory.
        if self
            .manifest
            .as_ref()
            .is_none_or(|m| m.desired.rootfs.is_none())
        {
            ctx.actor_ref()
                .stop_gracefully()
                .await
                .map_err(|err| err.to_string())?;
        }
        Ok(())
    }
}
#[remote_message]
impl Message<DeleteVM> for VMActor {
    type Reply = DeleteVMReply;
    async fn handle(
        &mut self,
        _msg: DeleteVM,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        trace!(vmid = %self.vmid, "Deleting VM actor and persistent rootfs state");
        self.starting = None;
        if let Err(err) = self.vm_instance.stop().await {
            return DeleteVMReply {
                error: Some(format!(
                    "VM stop failed; actor and rootfs retained for retry: {err}"
                )),
            };
        }
        self.started = false;
        let persistent = self.manifest.as_ref().is_some_and(|manifest| {
            manifest
                .desired
                .rootfs
                .as_ref()
                .is_some_and(|rootfs| rootfs.mode == crate::manifest::RootfsMode::Persistent)
        });
        if let Err(err) = teardown_rootfs(&mut self.virtiofs, &mut self.rootfs_mount).await {
            return DeleteVMReply {
                error: Some(format!(
                    "rootfs teardown failed; actor retained for retry: {err}"
                )),
            };
        }
        if persistent
            && let Err(err) =
                crate::ch_driver::containers::delete_persistent_rootfs(&self.vmid.to_string()).await
        {
            return DeleteVMReply {
                error: Some(format!(
                    "persistent rootfs deletion failed; actor retained for retry: {err}"
                )),
            };
        }
        if let Err(err) = self.vm_instance.purge_instance_data() {
            return DeleteVMReply {
                error: Some(format!(
                    "runtime cleanup failed; actor retained for retry: {err}"
                )),
            };
        }
        ctx.actor_ref().stop_gracefully().await.unwrap();
        DeleteVMReply { error: None }
    }
}
