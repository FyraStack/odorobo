use std::{collections::VecDeque, sync::Arc};

use crate::messages::vm::{
    DeleteVM, GetConsoleHistory, GetConsoleHistoryReply, GetVMHeartbeat, GetVMHeartbeatReply,
    GetVMInfo, GetVMInfoReply, MigrateVMReceive, MigrateVMReceiveReply, PrepMigration,
    SendConsoleInput, SendConsoleInputReply, ShutdownVM,
};
use crate::{
    ch_driver::{
        VMInstance,
        faas::{PreparedRootfs, VirtioFsSupervisor, VirtiofsdSpec},
        manifest::to_vm_config,
    },
    manifest::VmManifest,
};
use cloud_hypervisor_client::models::VmConfig;
use kameo::prelude::*;
use serde::{Deserialize, Serialize};
use stable_eyre::{
    Report, Result,
    eyre::eyre,
};
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

    /// Full FaaS boot through the actor (issue #112): OCI rootfs -> composefs
    /// mount -> supervised virtiofsd -> CH direct kernel boot -> guest mounts
    /// its root over virtiofs. Requires nested KVM, the cloud-hypervisor
    /// binary, virtiofsd and network access for the image pull, so it is
    /// ignored by default:
    /// `cargo test -p odorobo --bin odorobo faas_vm_boots -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "boots a real VM; needs /dev/kvm, cloud-hypervisor, virtiofsd and network for the OCI pull"]
    async fn faas_vm_boots_oci_rootfs_through_the_actor() {
        use crate::ch_driver::{
            VMInstance,
            faas,
            actor::VMActor,
        };
        use crate::manifest::{
            Boot, Compute, DesiredState, MANIFEST_VERSION, Metadata, Rootfs, VmManifest,
        };
        use crate::messages::vm::GetConsoleHistory;
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
                    // busybox has no /sbin/init; drop into its shell instead.
                    cmdline: Some(
                        "console=ttyS0 root=rootfs rootfstype=virtiofs rw init=/bin/sh".to_owned(),
                    ),
                    ..Default::default()
                },
                cloud_init: None,
                vsock: None,
                rootfs: Some(Rootfs {
                    oci: "docker://busybox:latest".to_owned(),
                    read_only: true,
                }),
            },
            observed: None,
        };

        // prepare() gives access to the ActorRef before the actor runs.
        let prepared = VMActor::prepare();
        let actor_ref = prepared.actor_ref().clone();
        let handle = prepared.spawn((vmid, Some(manifest)));

        // Wait for the guest kernel to mount the virtiofs root: the console
        // spool should show the kernel log mentioning virtiofs.
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let mut virtiofs_seen = false;
        while std::time::Instant::now() < deadline {
            let reply = actor_ref
                .ask(GetConsoleHistory { vmid })
                .await
                .expect("console history ask");
            let text = String::from_utf8_lossy(&reply.history);
            if text.contains("virtiofs") {
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
            "guest never mounted the virtiofs root within 120s"
        );

        // Graceful stop must tear down the whole tree: CH, the supervised
        // virtiofsd and the composefs mount.
        actor_ref
            .stop_gracefully()
            .await
            .expect("graceful stop");
        // The actor handle resolves once on_stop teardown has finished.
        let (actor, reason) = handle
            .await
            .expect("actor task")
            .expect("actor stopped cleanly");
        let _ = actor;
        info!(?reason, "faas boot test actor stopped");
        tokio::time::sleep(Duration::from_secs(1)).await;

        let socket = faas::socket_path_for(&vmid.to_string());
        let mount = VMInstance::runtime_dir_for(&vmid.to_string()).join(faas::ROOTFS_TAG);
        assert!(!socket.exists(), "virtiofsd socket leaked after stop");
        assert!(!mount.exists(), "rootfs mount point leaked after stop");
        let leaked = std::process::Command::new("pgrep")
            .arg("virtiofsd")
            .output()
            .expect("pgrep");
        assert!(!leaked.status.success(), "virtiofsd process leaked after actor stop");
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
}

impl Actor for VMActor {
    // The actor accepts intent; CH conversion happens inside on_start.
    type Args = (ulid::Ulid, Option<VmManifest>);
    type Error = Report;

    #[tracing::instrument(skip_all)]
    async fn on_start((vmid, vm_config): Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
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
        let mut virtiofs = None;
        let mut rootfs_mount = None;
        if let Some(manifest) = &vm_config
            && let Some(rootfs) = &manifest.desired.rootfs
        {
            let runtime_dir = VMInstance::runtime_dir_for(&vmid.to_string());
            // Resolve everything that can fail cheaply BEFORE mounting, so
            // early errors cannot leak a mounted rootfs.
            let program = crate::ch_driver::faas::virtiofsd_path().ok_or_else(|| {
                eyre!(
                    "virtiofsd not found (looked in PATH and /usr/libexec) — `dnf install virtiofsd`"
                )
            })?;
            let prepared = crate::ch_driver::faas::mount_rootfs(&vmid.to_string(), rootfs)
                .await
                .map_err(|err| {
                    Report::msg(format!("failed to prepare OCI rootfs: {err}"))
                })?;
            rootfs_mount = Some(prepared.clone());

            let spec = VirtiofsdSpec {
                program,
                socket: crate::ch_driver::faas::socket_path_for(&vmid.to_string()),
                shared_dir: prepared.mount.clone(),
                log: runtime_dir.join("virtiofsd.log"),
            };
            let supervisor = match VirtioFsSupervisor::start(spec) {
                Ok(supervisor) => supervisor,
                // Don't orphan the mount on this path.
                Err(err) => {
                    crate::ch_driver::faas::unmount_rootfs(&prepared).await;
                    return Err(err);
                }
            };
            if let Err(err) = supervisor
                .wait_socket_ready(std::time::Duration::from_secs(15))
                .await
            {
                // Don't orphan the supervisor or the mount on this path.
                supervisor.stop().await;
                crate::ch_driver::faas::unmount_rootfs(&prepared).await;
                return Err(err);
            }
            info!(%vmid, "virtiofsd supervised child started and socket ready");
            virtiofs = Some(supervisor);
        }

        let spawn_result =
            VMInstance::spawn(&vmid.to_string(), vm_config_for_ch, boot, None).await;
        if let Err(err) = spawn_result {
            // Don't orphan virtiofsd/the mount when CH fails to start.
            if let Some(supervisor) = virtiofs.take() {
                supervisor.stop().await;
            }
            if let Some(prepared) = rootfs_mount.take() {
                crate::ch_driver::faas::unmount_rootfs(&prepared).await;
            }
            return Err(err);
        }
        let mut vminstance = spawn_result?;

        let console = Console::default();
        // A migration receiver has no config yet; its serial socket is created
        // only when the migrated VM is restored.
        if vm_config.is_some() {
            let attach_result = console
                .attach_socket(vminstance.console_socket_path())
                .await;
            if let Err(err) = attach_result {
                if let Some(supervisor) = virtiofs.take() {
                    supervisor.stop().await;
                }
                if let Some(prepared) = rootfs_mount.take() {
                    crate::ch_driver::faas::unmount_rootfs(&prepared).await;
                }
                return Err(err);
            }
        }

        // Take the child process out so we can watch for unexpected death.
        // destroy() handles a missing child_process gracefully.
        if let Some(mut child_process) = vminstance.take_child_process() {
            let actor_ref = actor_ref.clone();
            tokio::spawn(async move {
                debug!(%vmid, "watching child process to handle actor cleanup");
                match child_process.wait().await {
                    Ok(status) => {
                        if status.success() {
                            warn!(%vmid, "child process exited outside of actor teardown");
                            _ = actor_ref.stop_gracefully().await;
                        } else {
                            error!(%vmid, ?status, "child process exited unexpectedly, killing actor");
                            actor_ref.kill();
                        }
                    }
                    Err(err) => {
                        error!(%vmid, ?err, "failed to wait on child process, killing actor");
                        actor_ref.kill();
                    }
                }
            });
        } else {
            warn!(%vmid, "VMInstance has no child process to watch");
        }

        Ok(Self {
            vmid,
            vm_instance: vminstance,
            migration_state: None,
            console,
            virtiofs,
            rootfs_mount,
            manifest: vm_config,
        })
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

        // Teardown order matters: stop the guest first (it stops issuing fs
        // requests), then the supervised virtiofsd, then unmount the
        // composefs rootfs — the mount point must be unmounted before
        // destroy() purges the runtime directory it lives in.
        if let Err(err) = self.vm_instance.shutdown().await {
            warn!(vmid = %self.vmid, ?err, "graceful VM shutdown before teardown failed; destroy() will retry");
        }
        if let Some(supervisor) = self.virtiofs.take() {
            supervisor.stop().await;
        }
        if let Some(prepared) = self.rootfs_mount.take() {
            crate::ch_driver::faas::unmount_rootfs(&prepared).await;
        }

        self.vm_instance.destroy().await?;

        // info!(vmid = %self.vmid, ?res, "VM process exited");

        Ok(())
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
                            attempt,
                            "serial console socket not ready during migration"
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
    type Reply = ();
    async fn handle(
        &mut self,
        _msg: ShutdownVM,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        trace!(vmid = %self.vmid, "Shutting down VM actor");
        ctx.actor_ref().stop_gracefully().await.unwrap();
        // ctx.actor_ref().kill();
    }
}
#[remote_message]
impl Message<DeleteVM> for VMActor {
    type Reply = ();
    async fn handle(
        &mut self,
        _msg: DeleteVM,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        trace!(vmid = %self.vmid, "Shutting down VM actor");
        ctx.actor_ref().stop_gracefully().await.unwrap();
    }
}
