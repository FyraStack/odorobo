use std::{
    collections::VecDeque,
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::messages::vm::{
    DeleteVM, GetConsoleHistory, GetConsoleHistoryReply, GetVMHeartbeat, GetVMHeartbeatReply,
    GetVMInfo, GetVMInfoReply, MigrateVMReceive, MigrateVMReceiveReply, SendConsoleInput,
    SendConsoleInputReply, ShutdownVM,
};
use crate::{
    ch_driver::{VMInstance, manifest::to_vm_config},
    manifest::VmManifest,
    vsock_cid::{PublishedRegistryUpdate, VsockCidAllocator, VsockCidReservation, VsockLeaseToken},
};
use kameo::prelude::*;
use serde::{Deserialize, Serialize};
use stable_eyre::{Report, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixStream, unix::OwnedWriteHalf},
    sync::{Mutex, broadcast},
    task::JoinHandle,
};
use tracing::{debug, error, info, trace, warn};

/// Cloud Hypervisor-specific state for an in-progress receive migration.
pub struct MigrationState {
    pub listening_address: String,
    /// The task handle for the migration process.
    pub migration_task: Option<JoinHandle<Result<()>>>,
    /// Set by the receive operation itself, before its completion notification.
    /// Teardown must commit a successful receive even if its mailbox is closed.
    receive_succeeded: Arc<AtomicBool>,
    completion_task: Option<JoinHandle<()>>,
    receive_abort: Option<tokio::task::AbortHandle>,
    console_attach_task: Option<JoinHandle<()>>,
    /// Reservation is committed only after the incoming migration completes.
    cid_reservation: Option<VsockCidReservation>,
    previous_manifest: Option<VmManifest>,
}

impl MigrationState {
    fn uncommitted_reservation(&mut self) -> Option<VsockCidReservation> {
        if self.receive_succeeded.load(Ordering::SeqCst) {
            self.cid_reservation = None;
            None
        } else {
            self.cid_reservation.take()
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("VM startup cleanup failed: {0}")]
pub struct FailedStartupCleanup(pub String);

/// Shared with the agent even when startup/teardown fails and no actor remains.
/// Process exit alone grants no authority over a lease acquired by another VMM.
#[derive(Debug)]
pub struct VMCleanupProof {
    process_exit_confirmed: AtomicBool,
    lease_token: std::sync::Mutex<Option<VsockLeaseToken>>,
}

impl VMCleanupProof {
    pub const fn new(process_exit_confirmed: bool) -> Self {
        Self {
            process_exit_confirmed: AtomicBool::new(process_exit_confirmed),
            lease_token: std::sync::Mutex::new(None),
        }
    }

    pub fn process_exit_confirmed(&self) -> bool {
        self.process_exit_confirmed.load(Ordering::SeqCst)
    }

    fn set_process_exit_confirmed(&self, confirmed: bool) {
        self.process_exit_confirmed
            .store(confirmed, Ordering::SeqCst);
    }

    pub fn lease_token(&self) -> Option<VsockLeaseToken> {
        *self
            .lease_token
            .lock()
            .expect("cleanup proof lock poisoned")
    }

    pub(crate) fn own_lease(&self, token: VsockLeaseToken) {
        *self
            .lease_token
            .lock()
            .expect("cleanup proof lock poisoned") = Some(token);
    }
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationFinished {
    pub succeeded: bool,
}

#[derive(RemoteActor)]
pub struct VMActor {
    pub vmid: ulid::Ulid,
    /// path to the Cloud Hypervisor socket, in /run/odorobo/vms/<VMID>/ch.sock
    pub vm_instance: VMInstance,
    pub migration_state: Option<MigrationState>,
    pub console: Console,
    /// Desired provider-neutral intent retained for VM info and migration.
    /// The translated Cloud Hypervisor config lives only in `VMInstance`.
    pub manifest: Option<VmManifest>,
    /// Explicit VM deletion releases any persistent vsock CID; shutdown keeps it.
    release_vsock_cid: bool,
    /// Exit and exact lease identity survive actor teardown for agent retries.
    cleanup_proof: Arc<VMCleanupProof>,
    /// Owns and supervises the child after it is transferred out of `VMInstance`.
    process_watcher: Option<(
        tokio::sync::oneshot::Sender<()>,
        JoinHandle<std::io::Result<ExitStatus>>,
    )>,
}

fn allocate_vsock_cid(
    vmid: ulid::Ulid,
    manifest: &mut VmManifest,
    use_observed_cid: bool,
    cleanup_proof: &VMCleanupProof,
) -> Result<Option<VsockCidReservation>> {
    manifest.validate()?;
    let observed_cid = use_observed_cid
        .then(|| {
            manifest
                .observed
                .as_ref()
                .and_then(|observed| observed.vsock_guest_cid)
        })
        .flatten();
    if let Some(vsock) = manifest.desired.vsock.as_ref() {
        let requested_cid = migration_vsock_cid(vsock.guest_cid, observed_cid, use_observed_cid)?;
        let allocator = VsockCidAllocator::from_environment();
        let reservation = allocator
            .reserve_with_previous(vmid, requested_cid)
            .map_err(|error| classify_cid_reservation_error(error, cleanup_proof))?;
        cleanup_proof.own_lease(reservation.token);
        manifest
            .observed
            .get_or_insert_with(Default::default)
            .vsock_guest_cid = Some(reservation.cid);
        return Ok(Some(reservation));
    }
    // Disabling vsock cannot evade an older incarnation's active lease after
    // an agent restart. Stopping this new VMM would not prove the old one exited.
    VsockCidAllocator::from_environment().ensure_no_active_lease(vmid)?;
    Ok(None)
}

fn classify_cid_reservation_error(error: Report, cleanup_proof: &VMCleanupProof) -> Report {
    if let Some(token) = error
        .downcast_ref::<PublishedRegistryUpdate>()
        .and_then(|published| published.reservation_token)
    {
        cleanup_proof.own_lease(token);
        error.wrap_err(FailedStartupCleanup(
            "vsock CID reservation was published but not durably synced".to_owned(),
        ))
    } else {
        // Validation/pre-publication failures may refer to another live
        // incarnation's lease. Never give the agent proof to release that one.
        error
    }
}

fn validate_migration_manifest(manifest: &VmManifest) -> Result<()> {
    if manifest.desired.cloud_init.is_some() {
        return Err(Report::msg(
            "live migration of cloud-init VMs is unsupported: destination seed paths cannot be overridden",
        ));
    }
    Ok(())
}

fn migration_vsock_cid(
    desired: Option<u32>,
    observed: Option<u32>,
    migrating: bool,
) -> Result<Option<u32>> {
    if !migrating {
        return Ok(desired);
    }
    if let (Some(desired), Some(observed)) = (desired, observed)
        && desired != observed
    {
        return Err(Report::msg(
            "migration desired and observed vsock CIDs disagree",
        ));
    }
    desired.or(observed).map(Some).ok_or_else(|| {
        Report::msg("vsock migration requires the source guest CID; automatic allocation is unsafe")
    })
}

/// Produce a temporary manifest for the provider converter without turning an
/// automatically allocated CID into desired intent stored by the scheduler.
fn manifest_with_resolved_vsock_cid(manifest: &VmManifest) -> VmManifest {
    let mut resolved = manifest.clone();
    if let Some(vsock) = resolved.desired.vsock.as_mut()
        && vsock.guest_cid.is_none()
    {
        vsock.guest_cid = resolved
            .observed
            .as_ref()
            .and_then(|observed| observed.vsock_guest_cid);
    }
    resolved
}

fn rollback_vsock_cid(
    allocator: &VsockCidAllocator,
    vmid: ulid::Ulid,
    reservation: Option<VsockCidReservation>,
) -> Result<()> {
    if let Some(reservation) = reservation {
        allocator.rollback(vmid, reservation).map_err(|error| {
            Report::new(FailedStartupCleanup(format!(
                "failed to roll back vsock CID reservation: {error}"
            )))
        })?;
    }
    Ok(())
}

fn finish_vsock_lease(
    allocator: &VsockCidAllocator,
    vmid: ulid::Ulid,
    lease_token: Option<VsockLeaseToken>,
    delete: bool,
) -> Result<()> {
    if let Some(token) = lease_token {
        if delete {
            allocator.release_owned(vmid, token)?;
        } else {
            allocator.mark_process_exited_owned(vmid, token)?;
        }
    } else if delete {
        // Deletion of a no-vsock incarnation may release a retained *stopped*
        // assignment, but never an older active lease whose exit we can't prove.
        allocator.release_stopped(vmid)?;
    }
    Ok(())
}

impl Actor for VMActor {
    // The actor accepts intent; CH conversion happens inside on_start.
    type Args = (ulid::Ulid, Option<VmManifest>, Arc<VMCleanupProof>);
    type Error = Report;

    #[tracing::instrument(skip_all)]
    #[allow(clippy::too_many_lines)]
    async fn on_start(
        (vmid, mut vm_config, cleanup_proof): Self::Args,
        actor_ref: ActorRef<Self>,
    ) -> Result<Self> {
        let cid_reservation = vm_config
            .as_mut()
            .map(|manifest| allocate_vsock_cid(vmid, manifest, false, &cleanup_proof))
            .transpose()?
            .flatten();
        // Boot is manifest intent, not a Cloud Hypervisor default. Preserve it
        // separately because VMInstance also supports create-without-boot paths.
        let boot = vm_config
            .as_ref()
            .is_some_and(|manifest| manifest.desired.boot.start);
        let runtime_dir = VMInstance::runtime_dir_for(&vmid.to_string());
        let vm_config_for_ch = match vm_config
            .as_ref()
            .map(manifest_with_resolved_vsock_cid)
            .as_ref()
            .map(|manifest| to_vm_config(manifest, &runtime_dir))
            .transpose()
        {
            Ok(config) => config,
            Err(error) => {
                rollback_vsock_cid(
                    &VsockCidAllocator::from_environment(),
                    vmid,
                    cid_reservation,
                )
                .map_err(|cleanup_error| cleanup_error.wrap_err(error.to_string()))?;
                return Err(error);
            }
        };
        cleanup_proof.set_process_exit_confirmed(false);
        let mut vminstance = match VMInstance::spawn(
            &vmid.to_string(),
            vm_config_for_ch,
            boot,
            None,
        )
        .await
        {
            Ok(instance) => instance,
            Err(error) => {
                if error
                    .downcast_ref::<super::instance::UnconfirmedProcessExit>()
                    .is_some()
                {
                    warn!(%vmid, ?error, "retaining runtime files and vsock lease because VMM exit was not confirmed");
                    return Err(error);
                }
                cleanup_proof.set_process_exit_confirmed(true);
                if error.downcast_ref::<FailedStartupCleanup>().is_some() {
                    return Err(error);
                }
                if let Err(cleanup_error) = std::fs::remove_dir_all(&runtime_dir)
                    && cleanup_error.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(Report::new(FailedStartupCleanup(format!(
                        "{error}; failed to clean VM runtime directory: {cleanup_error}"
                    ))));
                }
                rollback_vsock_cid(
                    &VsockCidAllocator::from_environment(),
                    vmid,
                    cid_reservation,
                )
                .map_err(|cleanup_error| cleanup_error.wrap_err(error.to_string()))?;
                return Err(error);
            }
        };

        let console = Console::default();
        // A migration receiver has no config yet; its serial socket is created
        // only when the migrated VM is restored.
        if vm_config.is_some()
            && let Err(error) = console
                .attach_socket(vminstance.console_socket_path())
                .await
        {
            match vminstance.destroy().await {
                Ok(()) => {
                    cleanup_proof.set_process_exit_confirmed(true);
                    rollback_vsock_cid(
                        &VsockCidAllocator::from_environment(),
                        vmid,
                        cid_reservation,
                    )
                    .map_err(|cleanup_error| cleanup_error.wrap_err(error.to_string()))?;
                }
                Err(cleanup_error) => {
                    if cleanup_error
                        .downcast_ref::<super::instance::UnconfirmedProcessExit>()
                        .is_none()
                    {
                        cleanup_proof.set_process_exit_confirmed(true);
                        if let Err(record_error) = finish_vsock_lease(
                            &VsockCidAllocator::from_environment(),
                            vmid,
                            cleanup_proof.lease_token(),
                            false,
                        ) {
                            warn!(%vmid, ?record_error, "failed to record process exit after console attachment failure");
                        }
                    }
                    warn!(%vmid, ?cleanup_error, "failed to clean VM after console attach failure; retaining vsock CID reservation");
                    return Err(Report::new(FailedStartupCleanup(format!(
                        "{error}; {cleanup_error}"
                    ))));
                }
            }
            return Err(error);
        }

        // Take the child process out so we can watch for unexpected death.
        // destroy() handles a missing child_process gracefully.
        let process_watcher = if let Some(mut child_process) = vminstance.take_child_process() {
            let (kill_sender, kill_receiver) = tokio::sync::oneshot::channel();
            let actor_ref = actor_ref.clone();
            let watcher = tokio::spawn(async move {
                debug!(%vmid, "watching child process to handle actor cleanup");
                let (result, teardown_requested) = tokio::select! {
                    result = child_process.wait() => (result, false),
                    _ = kill_receiver => {
                        // The process may already have exited. wait() reaps it
                        // either way; its result is the proof needed to release CID.
                        drop(child_process.start_kill());
                        (child_process.wait().await, true)
                    }
                };
                if !teardown_requested {
                    match &result {
                        Ok(status) if status.success() => {
                            warn!(%vmid, "child process exited outside of actor teardown");
                            let actor_ref = actor_ref.clone();
                            tokio::spawn(async move {
                                if let Err(error) = actor_ref.stop_gracefully().await {
                                    error!(%vmid, ?error, "failed to stop actor after VMM exit");
                                }
                            });
                        }
                        Ok(status) => {
                            error!(%vmid, ?status, "child process exited unexpectedly, killing actor");
                            actor_ref.kill();
                        }
                        Err(err) => {
                            error!(%vmid, ?err, "failed to wait on child process, killing actor");
                            actor_ref.kill();
                        }
                    }
                }
                result
            });
            Some((kill_sender, watcher))
        } else {
            warn!(%vmid, "VMInstance has no child process to watch");
            None
        };

        Ok(Self {
            vmid,
            vm_instance: vminstance,
            migration_state: None,
            console,
            manifest: vm_config,
            release_vsock_cid: false,
            cleanup_proof,
            process_watcher,
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

        if let Some(state) = self.migration_state.as_mut()
            && let Some(task) = state.console_attach_task.take()
        {
            task.abort();
        }
        let stop_result = self.vm_instance.stop().await;
        let process_exited = if let Some((kill_sender, watcher)) = self.process_watcher.take() {
            let _send_result: std::result::Result<(), ()> = kill_sender.send(());
            watcher
                .await
                .map_err(|error| Report::msg(format!("VM process watcher failed: {error}")))?
                .map_err(|error| {
                    Report::msg(format!("failed to confirm VM process exit: {error}"))
                })?;
            true
        } else {
            // Without a watcher there is no proof that the VMM process exited;
            // retain reservations rather than risk reassigning a live guest CID.
            false
        };

        // Reaping closes the receive connection. Let its task observe the
        // final response before deciding whether to roll back the reservation.
        if let Some(state) = self.migration_state.as_mut()
            && let Some(mut task) = state.completion_task.take()
            && tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
                .await
                .is_err()
        {
            if let Some(abort) = self
                .migration_state
                .as_mut()
                .and_then(|state| state.receive_abort.take())
            {
                abort.abort();
            }
            task.abort();
            drop(task.await);
        }

        let cleanup_result = if process_exited {
            self.cleanup_proof.set_process_exit_confirmed(true);
            self.vm_instance.cleanup_after_stop().await
        } else {
            Ok(())
        };

        if process_exited {
            let allocator = VsockCidAllocator::from_environment();
            let rollback_result = self.migration_state.take().map_or(Ok(()), |mut state| {
                rollback_vsock_cid(&allocator, self.vmid, state.uncommitted_reservation())
            });
            finish_vsock_lease(
                &allocator,
                self.vmid,
                self.cleanup_proof.lease_token(),
                self.release_vsock_cid,
            )?;
            rollback_result?;
        }

        stop_result?;
        cleanup_result
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

    #[allow(
        clippy::too_many_lines,
        reason = "keeps migration reservation and receiver setup ordered"
    )]
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

        if let Err(error) = validate_migration_manifest(&msg.config) {
            return MigrateVMReceiveReply {
                listening_address: String::new(),
                error: Some(error.to_string()),
            };
        }
        let mut prep_manifest = msg.config.clone();
        let cid_reservation =
            match allocate_vsock_cid(self.vmid, &mut prep_manifest, true, &self.cleanup_proof) {
                Ok(reservation) => reservation,
                Err(error) => {
                    // allocate_vsock_cid retains only the exact published token.
                    // Rejection of another incarnation never grants ownership.
                    return MigrateVMReceiveReply {
                        listening_address: String::new(),
                        error: Some(error.to_string()),
                    };
                }
            };

        // Keep ownership of this reservation in the actor until the process
        // exits. Any failure below tears down the receiver and rolls back from
        // on_stop, after the process watcher confirms exit.
        let receive_succeeded = Arc::new(AtomicBool::new(false));
        self.migration_state = Some(MigrationState {
            migration_task: None,
            receive_succeeded: Arc::clone(&receive_succeeded),
            completion_task: None,
            receive_abort: None,
            console_attach_task: None,
            listening_address: String::new(),
            cid_reservation,
            previous_manifest: self.manifest.clone(),
        });

        // Translate before opening a receive socket so invalid intent cannot
        // leave behind a migration listener that can never complete.
        let runtime_dir = VMInstance::runtime_dir_for(&self.vmid.to_string());
        let resolved_manifest = manifest_with_resolved_vsock_cid(&prep_manifest);
        let config = match to_vm_config(&resolved_manifest, &runtime_dir) {
            Ok(config) => config,
            Err(error) => {
                ctx.stop();
                return MigrateVMReceiveReply {
                    listening_address: String::new(),
                    error: Some(error.to_string()),
                };
            }
        };

        // Perform every fallible preparation step before opening the receiver.
        // On failure, stop the actor so on_stop rolls back after process exit.
        if let Err(error) = self.vm_instance.prep_config(config.clone()).await {
            ctx.stop();
            return MigrateVMReceiveReply {
                listening_address: String::new(),
                error: Some(error.to_string()),
            };
        }

        // Start receiving migration on the destination VM (this actor).
        let (listening_address, migration_task) =
            match self.vm_instance.receive_migration(receive_succeeded).await {
                Ok(result) => result,
                Err(error) => {
                    ctx.stop();
                    return MigrateVMReceiveReply {
                        listening_address: String::new(),
                        error: Some(error.to_string()),
                    };
                }
            };

        self.manifest = Some(prep_manifest);
        if let Some(state) = self.migration_state.as_mut() {
            state.listening_address.clone_from(&listening_address);
            state.migration_task = Some(migration_task);
        }

        let console = self.console.clone();
        let console_socket_path = self.vm_instance.console_socket_path();
        let console_attach_task = tokio::spawn(async move {
            loop {
                match console.attach_socket(console_socket_path.clone()).await {
                    Ok(()) => break,
                    Err(err) => {
                        trace!(?err, "serial console socket not ready during migration");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        });

        if let Some(state) = self.migration_state.as_mut() {
            state.console_attach_task = Some(console_attach_task);
        }

        // Notify the VM actor after receive finishes so it can commit the CID
        // reservation or stop and roll it back after process exit.
        if let Some(migration_state) = self.migration_state.as_mut()
            && let Some(migration_task) = migration_state.migration_task.take()
        {
            let actor_ref = ctx.actor_ref().clone();
            migration_state.receive_abort = Some(migration_task.abort_handle());
            migration_state.completion_task = Some(tokio::spawn(async move {
                let succeeded = match migration_task.await {
                    Ok(Ok(())) => true,
                    Ok(Err(error)) => {
                        error!(?error, "migration receiver failed");
                        false
                    }
                    Err(error) => {
                        error!(?error, "migration receiver task failed");
                        false
                    }
                };
                if let Err(error) = actor_ref.tell(MigrationFinished { succeeded }).await {
                    error!(?error, "failed to notify actor that migration finished");
                }
            }));
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
        msg: MigrationFinished,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if msg.succeeded {
            if let Some(mut state) = self.migration_state.take() {
                if let Some(task) = state.console_attach_task.take() {
                    task.abort();
                }
                if let Err(error) = self
                    .console
                    .attach_socket(self.vm_instance.console_socket_path())
                    .await
                {
                    warn!(vmid = %self.vmid, ?error, "failed to attach console after migration");
                }
                info!(vmid = %self.vmid, "migration finished successfully");
            } else {
                warn!(vmid = %self.vmid, "received migration finished notification with no active migration state");
            }
        } else if let Some(state) = self.migration_state.as_ref() {
            self.manifest.clone_from(&state.previous_manifest);
            warn!(vmid = %self.vmid, "migration failed; stopping receiver before rolling back CID reservation");
            ctx.stop();
        } else {
            warn!(vmid = %self.vmid, "received migration failed notification with no active migration state");
        }
    }
}

#[cfg(test)]
mod vsock_manifest_tests {
    use super::{
        FailedStartupCleanup, MigrationState, VMCleanupProof, classify_cid_reservation_error,
        finish_vsock_lease, manifest_with_resolved_vsock_cid, migration_vsock_cid,
        rollback_vsock_cid, validate_migration_manifest,
    };
    use crate::manifest::{ObservedState, ObservedStatus, VmManifest};
    use crate::vsock_cid::VsockCidAllocator;

    #[test]
    fn failed_cid_rollback_is_quarantined_and_recoverable_after_registry_repair() {
        let directory = std::env::temp_dir().join(format!(
            "odorobo-rollback-failure-{}",
            ulid::Ulid::generate()
        ));
        let registry_path = directory.join("registry.json");
        let allocator = VsockCidAllocator::new(&registry_path);
        let vmid = ulid::Ulid::generate();
        let reservation = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        let registry = std::fs::read(&registry_path).unwrap();
        // An unreadable registry prevents completing rollback, independently
        // of whether the tests run as root or as an unprivileged user.
        std::fs::write(&registry_path, b"invalid registry").unwrap();
        let error = rollback_vsock_cid(&allocator, vmid, Some(reservation)).unwrap_err();
        assert!(error.downcast_ref::<FailedStartupCleanup>().is_some());
        std::fs::write(&registry_path, registry).unwrap();
        // The agent's quarantine retains the confirmed-exit proof, allowing
        // explicit deletion to repair the persisted lease after storage recovers.
        allocator
            .mark_process_exited_owned(vmid, reservation.token)
            .unwrap();
        allocator.release_owned(vmid, reservation.token).unwrap();
        assert_eq!(
            allocator.reserve_with_previous(vmid, Some(42)).unwrap().cid,
            42
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejected_receiver_exit_does_not_mark_older_active_lease_stopped() {
        let directory = std::env::temp_dir().join(format!(
            "odorobo-rejected-receiver-{}",
            ulid::Ulid::generate()
        ));
        let allocator = VsockCidAllocator::new(directory.join("registry.json"));
        let vmid = ulid::Ulid::generate();
        let reservation = allocator.reserve_with_previous(vmid, Some(47)).unwrap();
        drop(allocator.reserve_with_previous(vmid, Some(47)).unwrap_err());
        finish_vsock_lease(&allocator, vmid, None, false).unwrap();
        drop(allocator.release_stopped(vmid).unwrap_err());
        drop(finish_vsock_lease(&allocator, vmid, None, true).unwrap_err());
        drop(allocator.ensure_no_active_lease(vmid).unwrap_err());
        finish_vsock_lease(&allocator, vmid, Some(reservation.token), false).unwrap();
        allocator.ensure_no_active_lease(vmid).unwrap();
        assert!(allocator.release_stopped(vmid).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ordinary_reservation_rejection_never_claims_cleanup_ownership() {
        let proof = VMCleanupProof::new(true);
        let error =
            classify_cid_reservation_error(stable_eyre::Report::msg("active lease"), &proof);
        assert!(error.downcast_ref::<FailedStartupCleanup>().is_none());
        assert_eq!(proof.lease_token(), None);
    }

    #[test]
    fn postpublication_failure_retains_owned_token_and_cleanup_marker_for_agent() {
        let directory = std::env::temp_dir().join(format!(
            "odorobo-published-ownership-{}",
            ulid::Ulid::generate()
        ));
        let allocator = VsockCidAllocator::new(directory.join("registry.json"));
        let vmid = ulid::Ulid::generate();
        let reservation = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        let proof = VMCleanupProof::new(true);
        let error = classify_cid_reservation_error(
            stable_eyre::Report::new(crate::vsock_cid::PublishedRegistryUpdate {
                source: std::io::Error::other("directory sync failed"),
                reservation_token: Some(reservation.token),
            })
            .wrap_err("reserve failed"),
            &proof,
        );
        assert!(error.downcast_ref::<FailedStartupCleanup>().is_some());
        assert!(
            error
                .downcast_ref::<crate::vsock_cid::PublishedRegistryUpdate>()
                .is_some()
        );
        assert!(proof.process_exit_confirmed());
        assert_eq!(proof.lease_token(), Some(reservation.token));
        finish_vsock_lease(&allocator, vmid, proof.lease_token(), false).unwrap();
        finish_vsock_lease(&allocator, vmid, proof.lease_token(), true).unwrap();
        assert!(!allocator.release_stopped(vmid).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn local_stop_returns_reply_with_a_full_bounded_mailbox() {
        use kameo::prelude::*;
        use std::sync::Arc;
        use tokio::sync::Notify;

        struct StopActor;
        impl Actor for StopActor {
            type Args = Self;
            type Error = stable_eyre::Report;
            async fn on_start(args: Self, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
                Ok(args)
            }
        }
        struct StopAfterPreparation {
            entered: Arc<Notify>,
            proceed: Arc<Notify>,
        }
        impl Message<StopAfterPreparation> for StopActor {
            type Reply = ();
            async fn handle(
                &mut self,
                msg: StopAfterPreparation,
                ctx: &mut Context<Self, Self::Reply>,
            ) {
                msg.entered.notify_one();
                msg.proceed.notified().await;
                // The same non-enqueueing stop used by VM migration failures.
                ctx.stop();
            }
        }
        impl Message<()> for StopActor {
            type Reply = ();
            async fn handle(&mut self, _msg: (), _ctx: &mut Context<Self, Self::Reply>) {}
        }
        let actor = StopActor::spawn_with_mailbox(StopActor, kameo::mailbox::bounded(1));
        actor.wait_for_startup().await;
        let entered = Arc::new(Notify::new());
        let proceed = Arc::new(Notify::new());
        let requester = actor.clone();
        let request = StopAfterPreparation {
            entered: Arc::clone(&entered),
            proceed: Arc::clone(&proceed),
        };
        let reply = tokio::spawn(async move { requester.ask(request).await });
        entered.notified().await;
        actor.tell(()).await.unwrap();
        proceed.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            reply.await.unwrap().unwrap();
            actor.wait_for_shutdown().await;
        })
        .await
        .unwrap();
    }

    #[test]
    fn successful_receive_commits_cid_before_notification_is_delivered() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let directory = std::env::temp_dir().join(format!(
            "odorobo-migration-commit-{}",
            ulid::Ulid::generate()
        ));
        let allocator = VsockCidAllocator::new(directory.join("registry.json"));
        let vmid = ulid::Ulid::generate();
        let reservation = allocator.reserve_with_previous(vmid, Some(47)).unwrap();
        let receive_succeeded = Arc::new(AtomicBool::new(false));
        let mut state = MigrationState {
            listening_address: String::new(),
            migration_task: None,
            receive_succeeded: Arc::clone(&receive_succeeded),
            completion_task: None,
            receive_abort: None,
            console_attach_task: None,
            cid_reservation: Some(reservation),
            previous_manifest: None,
        };
        // The receive worker has finished but its notification is gated/closed.
        receive_succeeded.store(true, Ordering::SeqCst);
        rollback_vsock_cid(&allocator, vmid, state.uncommitted_reservation()).unwrap();
        finish_vsock_lease(&allocator, vmid, Some(reservation.token), false).unwrap();
        assert_eq!(allocator.reserve_with_previous(vmid, None).unwrap().cid, 47);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cloud_init_migration_is_rejected_before_creating_artifacts() {
        let manifest: VmManifest = serde_json::from_str(include_str!(
            "../../../docs/fixtures/manifest/cloud-init.json"
        ))
        .unwrap();
        drop(validate_migration_manifest(&manifest).unwrap_err());
    }

    #[test]
    fn migration_requires_and_preserves_the_source_vsock_cid() {
        drop(migration_vsock_cid(None, None, true).unwrap_err());
        drop(migration_vsock_cid(Some(42), Some(43), true).unwrap_err());
        assert_eq!(migration_vsock_cid(None, Some(42), true).unwrap(), Some(42));
        assert_eq!(migration_vsock_cid(Some(42), None, true).unwrap(), Some(42));
        assert_eq!(migration_vsock_cid(None, Some(42), false).unwrap(), None);
    }

    #[test]
    fn resolved_automatic_cid_does_not_become_desired_intent() {
        let mut manifest: VmManifest =
            serde_json::from_str(include_str!("../../../docs/fixtures/manifest/vsock.json"))
                .expect("vsock fixture parses");
        manifest.desired.vsock.as_mut().unwrap().guest_cid = None;
        manifest.observed = Some(ObservedState {
            status: ObservedStatus::Running,
            vsock_guest_cid: Some(47),
            ..Default::default()
        });

        let resolved = manifest_with_resolved_vsock_cid(&manifest);

        assert_eq!(manifest.desired.vsock.unwrap().guest_cid, None);
        assert_eq!(resolved.desired.vsock.unwrap().guest_cid, Some(47));
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
        ctx.stop();
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
        // Explicit deletion also releases a lease retained by an earlier
        // incarnation, even if this incarnation no longer has a vsock device.
        self.release_vsock_cid = true;
        ctx.stop();
    }
}
