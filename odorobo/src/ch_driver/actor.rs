use std::{collections::VecDeque, sync::Arc};

use crate::messages::vm::{
    DeleteVM, DeleteVMReply, GetConsoleHistory, GetConsoleHistoryReply, GetVMHeartbeat,
    GetVMHeartbeatReply, GetVMInfo, GetVMInfoReply, MigrateVMReceive, MigrateVMReceiveReply,
    PrepMigration, SendConsoleInput, SendConsoleInputReply, ShutdownVM,
};
use crate::{
    ch_driver::{
        VMInstance,
        containers::{PreparedRootfs, VirtioFsSupervisor, VirtiofsdSpec},
        manifest::to_vm_config,
    },
    manifest::VmManifest,
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

    /// Return the currently retained serial output, oldest bytes first.
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
        let failure = actor.ask(super::StartVM).await.unwrap_err().to_string();
        assert!(
            failure.contains("skopeo"),
            "fixture must reach OCI acquisition: {failure}"
        );
        assert!(
            actor
                .ask(crate::messages::vm::GetVMInfo { vmid: Some(vmid) })
                .await
                .unwrap()
                .config
                .is_some()
        );
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

    /// Full OCI rootfs boot through the actor: OCI root -> composefs layer
    /// mounts -> supervised virtiofsd -> CH direct kernel -> virtiofs root.
    /// Requires nested KVM, the host tools, and Zig for a static init stub, so
    /// it is ignored by default: `cargo test -p odorobo --bin odorobo faas_vm_boots -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "boots a real VM; needs /dev/kvm, cloud-hypervisor, virtiofsd, skopeo, composefs host support, and zig"]
    async fn faas_vm_boots_oci_rootfs_through_the_actor() {
        use crate::ch_driver::{VMInstance, actor::VMActor, containers};
        use crate::manifest::{
            Boot, Compute, DesiredState, MANIFEST_VERSION, Metadata, Rootfs, VmManifest,
        };
        use crate::messages::vm::{DeleteVM, GetConsoleHistory};
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
        let oci_layout = crate::ch_driver::containers::tests::fixture_oci_layout_with_init(
            &fixture_dir,
            &init_binary,
        );
        let image_ref = format!("oci:{}:latest", oci_layout.display());
        let manifest = VmManifest {
            api_version: MANIFEST_VERSION,
            id: vmid,
            desired: DesiredState {
                metadata: Metadata {
                    name: "faas-boot-test".to_owned(),
                    ..Default::default()
                },
                compute: Compute {
                    vcpus: 1,
                    memory_bytes: 512 * 1024 * 1024,
                    ..Default::default()
                },
                storage: vec![],
                networks: vec![],
                placement: Default::default(),
                boot: Boot {
                    start: true,
                    // The fixture installs our static init at /bin/sh.
                    cmdline: Some(
                        "console=ttyS0 root=rootfs rootfstype=virtiofs rw init=/bin/sh".to_owned(),
                    ),
                    ..Default::default()
                },
                cloud_init: None,
                vsock: None,
                rootfs: Some(Rootfs {
                    oci: image_ref,
                    mode: crate::manifest::RootfsMode::Persistent,
                }),
            },
            observed: None,
        };

        // prepare() gives access to the ActorRef before the actor runs.
        let prepared = VMActor::prepare();
        let actor_ref = prepared.actor_ref().clone();
        let handle = prepared.spawn((vmid, Some(manifest)));

        // Require userspace execution, not merely a kernel driver log.
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let mut virtiofs_seen = false;
        while std::time::Instant::now() < deadline {
            let reply = match actor_ref.ask(GetConsoleHistory { vmid }).await {
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
            let text = String::from_utf8_lossy(&reply.history);
            if text.contains("odorobo-test-init") {
                virtiofs_seen = true;
                info!(
                    vmid = %vmid,
                    tail = %String::from_utf8_lossy(&reply.history
                        [reply.history.len().saturating_sub(400)..]),
                    "guest console shows virtiofs"
                );
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert!(
            virtiofs_seen,
            "guest never executed fixture init within 120s"
        );

        // Cold stop must detach mounts but keep the lifecycle owner and upper
        // reachable. Deletion must not need the image or a new hypervisor.
        let persistent = PathBuf::from("/var/lib/odorobo/persistent-rootfs").join(vmid.to_string());
        std::fs::write(persistent.join("rootfs.upper/marker"), b"retained").unwrap();
        // Force a post-VMM-stop cleanup failure. A restart must not succeed
        // through a stale 'started' flag while the old layer is still busy.
        let layer_root = VMInstance::runtime_dir_for(&vmid.to_string()).join("rootfs-layers");
        let layer = std::fs::read_dir(&layer_root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut busy = tokio::process::Command::new("sleep")
            .arg("100")
            .current_dir(&layer)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        assert!(
            actor_ref
                .ask(crate::messages::vm::ShutdownVM { vmid })
                .await
                .is_err()
        );
        assert!(
            actor_ref.ask(super::StartVM).await.is_err(),
            "restart must retry retained teardown"
        );
        busy.kill().await.unwrap();
        actor_ref
            .ask(super::StartVM)
            .await
            .expect("restart after unblocking teardown");
        // Deliver a predecessor's queued exit only after replacement startup.
        actor_ref
            .ask(super::HypervisorExited { generation: 1 })
            .await
            .unwrap();
        actor_ref.ask(super::CheckRunning).await.unwrap();
        let layer = std::fs::read_dir(&layer_root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut busy = tokio::process::Command::new("sleep")
            .arg("100")
            .current_dir(layer)
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
            "crash must retain busy-layer cleanup owner"
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
        let (actor, reason) = handle
            .await
            .expect("actor task")
            .expect("actor stopped cleanly");
        let _ = actor;
        info!(?reason, "faas boot test actor stopped");
        tokio::time::sleep(Duration::from_secs(1)).await;

        let socket = containers::socket_path_for(&vmid.to_string());
        let mount = VMInstance::runtime_dir_for(&vmid.to_string()).join(containers::ROOTFS_TAG);
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationFinished;

#[derive(RemoteActor)]
pub struct VMActor {
    pub vmid: ulid::Ulid,
    /// path to the Cloud Hypervisor socket, in /run/odorobo/vms/<VMID>/ch.sock
    pub vm_instance: VMInstance,
    pub migration_state: Option<MigrationState>,
    pub console: Console,
    /// Supervised virtiofsd child (extra process supervised by this actor)
    /// when the VM boots an OCI rootfs over virtiofs (FaaS, issue #112).
    pub virtiofs: Option<VirtioFsSupervisor>,
    /// This VM's digest-pinned composefs mount of its OCI rootfs.
    pub rootfs_mount: Option<PreparedRootfs>,
    /// Desired provider-neutral intent retained for VM info and migration.
    /// The translated Cloud Hypervisor config lives only in `VMInstance`.
    pub manifest: Option<VmManifest>,
    /// OCI lifecycle owners survive failed startup and cold stop, so cleanup
    /// remains reachable without pulling or booting the image again.
    startup_error: Option<String>,
    started: bool,
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

#[remote_message]
impl Message<StartVM> for VMActor {
    type Reply = Result<(), String>;

    async fn handle(&mut self, _msg: StartVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        if self.started {
            return Ok(());
        }
        if let Err(err) = self.start(ctx.actor_ref().clone()).await {
            let cleanup = async {
                self.vm_instance.stop().await?;
                teardown_rootfs(&mut self.virtiofs, &mut self.rootfs_mount).await?;
                self.vm_instance.purge_instance_data()
            }
            .await;
            let message = format!("{err:#}; cleanup: {cleanup:?}");
            self.startup_error = Some(message.clone());
            return Err(message);
        }
        Ok(())
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

impl VMActor {
    async fn start(&mut self, actor_ref: ActorRef<Self>) -> Result<()> {
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

        // FaaS rootfs (issue #112): prepare the composefs mount and start the
        // supervised virtiofsd BEFORE spawning CH — the fs device connects to
        // the vhost-user socket at VM create time. Everything started here is
        // cleaned up on any later failure.
        let virtiofs = &mut self.virtiofs;
        let rootfs_mount = &mut self.rootfs_mount;
        if let Some(manifest) = &vm_config
            && let Some(rootfs) = &manifest.desired.rootfs
        {
            let runtime_dir = VMInstance::runtime_dir_for(&vmid.to_string());
            // Resolve everything that can fail cheaply BEFORE mounting, so
            // early errors cannot leak a mounted rootfs.
            let program = crate::ch_driver::containers::virtiofsd_path().ok_or_else(|| {
                eyre!(
                    "virtiofsd not found (looked in PATH and /usr/libexec) — `dnf install virtiofsd`"
                )
            })?;
            crate::ch_driver::containers::mount_rootfs_owned(
                &vmid.to_string(),
                rootfs,
                rootfs_mount,
            )
            .await?;
            let spec = VirtiofsdSpec {
                program,
                socket: crate::ch_driver::containers::socket_path_for(&vmid.to_string()),
                shared_dir: rootfs_mount.as_ref().unwrap().mount.clone(),
                log: runtime_dir.join("virtiofsd.log"),
            };
            let start = async {
                *virtiofs = Some(VirtioFsSupervisor::start(spec)?);
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
        }

        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| eyre!("hypervisor generation overflow"))?;
        VMInstance::spawn_into(
            &mut self.vm_instance,
            &vmid.to_string(),
            vm_config_for_ch,
            boot,
            None,
        )
        .await?;

        let console = Console::default();
        // A migration receiver has no config yet; its serial socket is created
        // only when the migrated VM is restored.
        if vm_config.is_some() {
            let attach_result = console
                .attach_socket(self.vm_instance.console_socket_path())
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
            virtiofs: None,
            rootfs_mount: None,
            manifest,
            startup_error: None,
            started: false,
            generation: 0,
        };
        if let Err(err) = actor.start(actor_ref).await {
            let cleanup = async {
                actor.vm_instance.stop().await?;
                teardown_rootfs(&mut actor.virtiofs, &mut actor.rootfs_mount).await?;
                actor.vm_instance.purge_instance_data()
            }
            .await;
            let message = format!("{err:#}; cleanup: {cleanup:?}");
            error!(%vmid, %message, "VM startup failed; retaining lifecycle owner for retry or delete");
            actor.startup_error = Some(message);
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
impl Message<SendConsoleInput> for VMActor {
    type Reply = SendConsoleInputReply;

    async fn handle(
        &mut self,
        msg: SendConsoleInput,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let written = msg.input.len();
        match self.console.write_input(&msg.input).await {
            Ok(()) => SendConsoleInputReply {
                written,
                error: None,
            },
            Err(err) => {
                error!(vmid = %self.vmid, ?err, "failed to write to serial console");
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
        GetVMInfoReply {
            vmid: self.vmid,
            config: self.manifest.clone(),
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

        // FaaS rootfs VMs are not migratable: the virtiofs device is
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
        if !self
            .manifest
            .as_ref()
            .is_some_and(|m| m.desired.rootfs.is_some())
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
        if persistent {
            if let Err(err) =
                crate::ch_driver::containers::delete_persistent_rootfs(&self.vmid.to_string()).await
            {
                return DeleteVMReply {
                    error: Some(format!(
                        "persistent rootfs deletion failed; actor retained for retry: {err}"
                    )),
                };
            }
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
