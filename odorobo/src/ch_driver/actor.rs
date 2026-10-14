use std::{collections::VecDeque, sync::Arc};

use crate::messages::vm::{
    DeleteVM, GetConsoleHistory, GetConsoleHistoryReply, GetVMHeartbeat, GetVMHeartbeatReply,
    GetVMInfo, GetVMInfoReply, MigrateVMReceive, MigrateVMReceiveReply, PrepMigration,
    SendConsoleInput, SendConsoleInputReply, ShutdownVM, ShutdownVMReply,
};
use crate::{
    ch_driver::{VMInstance, manifest::to_vm_config},
    manifest::VmManifest,
};
use cloud_hypervisor_client::models::VmConfig;
use kameo::prelude::*;
use serde::{Deserialize, Serialize};
use stable_eyre::{Report, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
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
    spool_task: Arc<Mutex<ConsoleTask>>,
}

#[derive(Default)]
struct ConsoleTask {
    handle: Option<JoinHandle<()>>,
    stopped: bool,
}

impl Drop for ConsoleTask {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl Default for Console {
    fn default() -> Self {
        let (output, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Mutex::new(ConsoleBuffer::default())),
            output,
            writer: Arc::new(Mutex::new(None)),
            spool_task: Arc::new(Mutex::new(ConsoleTask::default())),
        }
    }
}

#[derive(Default)]
struct ConsoleBuffer {
    ring: VecDeque<Vec<u8>>,
    len: usize,
}

impl Console {
    /// Attach to a Cloud Hypervisor serial socket and keep the spool connected
    /// across guest shutdown and boot cycles.
    pub async fn attach_socket(&self, socket_path: std::path::PathBuf) -> Result<()> {
        self.start_spooling(socket_path, false).await
    }

    /// Migration receivers do not have a serial endpoint until restore finishes.
    /// Use the same owned task for initial connection and subsequent reconnects.
    async fn wait_for_socket(&self, socket_path: std::path::PathBuf) -> Result<()> {
        self.start_spooling(socket_path, true).await
    }

    async fn start_spooling(&self, socket_path: std::path::PathBuf, wait: bool) -> Result<()> {
        let mut task_guard = self.spool_task.lock().await;
        if task_guard.stopped {
            return Err(Report::msg("console spool has been stopped"));
        }
        if task_guard
            .handle
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return Ok(());
        }

        let reader = if wait {
            None
        } else {
            let stream = UnixStream::connect(&socket_path).await.map_err(|err| {
                Report::msg(format!(
                    "failed to attach console spool to {}: {err}",
                    socket_path.display()
                ))
            })?;
            let (reader, writer) = stream.into_split();
            *self.writer.lock().await = Some(writer);
            Some(reader)
        };

        // Do not let the task retain its own task owner: dropping the last
        // external Console must abort the spool and close its socket halves.
        let spool = Self {
            inner: Arc::clone(&self.inner),
            output: self.output.clone(),
            writer: Arc::clone(&self.writer),
            spool_task: Arc::new(Mutex::new(ConsoleTask::default())),
        };
        task_guard.handle = Some(tokio::spawn(async move {
            spool.spool_socket(socket_path, reader).await;
        }));
        drop(task_guard);
        Ok(())
    }

    #[expect(
        clippy::infinite_loop,
        reason = "the reconnecting console task runs until its VM actor is torn down"
    )]
    async fn spool_socket(
        &self,
        socket_path: std::path::PathBuf,
        mut reader: Option<OwnedReadHalf>,
    ) {
        let mut buffer = vec![0_u8; 16 * 1024].into_boxed_slice();
        loop {
            if let Some(current_reader) = reader.as_mut() {
                match current_reader.read(&mut buffer).await {
                    Ok(0) => debug!("serial console closed; reconnecting"),
                    Ok(read) => {
                        self.push(buffer[..read].to_vec()).await;
                        continue;
                    }
                    Err(err) => warn!(?err, "serial console spool stopped reading; reconnecting"),
                }
            }

            reader.take();
            self.writer.lock().await.take();
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            match UnixStream::connect(&socket_path).await {
                Ok(stream) => {
                    let (next_reader, next_writer) = stream.into_split();
                    *self.writer.lock().await = Some(next_writer);
                    reader = Some(next_reader);
                    info!(path = %socket_path.display(), "serial console reconnected");
                }
                Err(err) => {
                    trace!(?err, path = %socket_path.display(), "serial console is not available yet");
                }
            }
        }
    }

    /// Stop the reconnecting console task when its VM actor is torn down.
    async fn stop_spooling(&self) {
        let task = {
            let mut state = self.spool_task.lock().await;
            state.stopped = true;
            state.handle.take()
        };
        if let Some(task) = task {
            task.abort();
            task.await.unwrap_or(());
        }
        self.writer.lock().await.take();
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
            let result = writer.write_all(input).await;
            if result.is_err() {
                writer_guard.take();
            }
            drop(writer_guard);
            result
                .map_err(|error| Report::msg(format!("failed to write to serial console: {error}")))
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
    use std::time::Duration;

    use super::{CONSOLE_SPOOL_SIZE, Console};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };

    async fn wait_for_history(console: &Console, expected_suffix: &[u8]) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if console.history().await.ends_with(expected_suffix) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("console output should reach the spool");
    }

    #[tokio::test]
    async fn console_reconnects_after_socket_disconnect() {
        let socket_path =
            std::env::temp_dir().join(format!("odorobo-console-{}.sock", ulid::Ulid::generate()));
        let listener = UnixListener::bind(&socket_path).expect("bind test console socket");
        let console = Console::default();
        console
            .attach_socket(socket_path.clone())
            .await
            .expect("attach console spool");

        let (mut first_connection, _) = listener.accept().await.expect("first connection");
        first_connection
            .write_all(b"before shutdown")
            .await
            .expect("write first console output");
        wait_for_history(&console, b"before shutdown").await;
        drop(first_connection);
        drop(listener);
        std::fs::remove_file(&socket_path).expect("remove stopped serial endpoint");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if console.writer.lock().await.is_none() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stale console writer should be cleared");
        assert!(console.write_input(b"disconnected").await.is_err());
        // Keep the endpoint absent through at least one failed reconnect.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let listener =
            UnixListener::bind(&socket_path).expect("recreate serial endpoint after boot");

        let (mut second_connection, _) =
            tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .expect("console spool should reconnect")
                .expect("accept reconnected console");
        second_connection
            .write_all(b"after boot")
            .await
            .expect("write reconnected console output");
        wait_for_history(&console, b"after boot").await;

        console
            .write_input(b"hello")
            .await
            .expect("input should use reconnected socket");
        let mut input = [0; 5];
        tokio::time::timeout(
            Duration::from_secs(1),
            second_connection.read_exact(&mut input),
        )
        .await
        .expect("console input should arrive")
        .expect("read console input");
        assert_eq!(&input, b"hello");

        console.stop_spooling().await;
        drop(second_connection);
        drop(listener);
        std::fs::remove_file(socket_path).expect("remove test console socket");
    }

    #[tokio::test]
    async fn dropping_console_cancels_its_spool_task() {
        let socket_path =
            std::env::temp_dir().join(format!("odorobo-drop-{}.sock", ulid::Ulid::generate()));
        let listener = UnixListener::bind(&socket_path).expect("bind console socket");
        let console = Console::default();
        let inner = std::sync::Arc::downgrade(&console.inner);
        console
            .attach_socket(socket_path.clone())
            .await
            .expect("attach console");
        let (mut connection, _) = listener.accept().await.expect("accept console");
        drop(console);
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), connection.read(&mut byte))
                .await
                .expect("spool should close socket")
                .expect("read socket"),
            0
        );
        assert!(inner.upgrade().is_none());
        std::fs::remove_file(socket_path).expect("remove test socket");
    }

    #[tokio::test]
    async fn stopping_console_cancels_initial_migration_connection_and_prevents_restart() {
        let socket_path =
            std::env::temp_dir().join(format!("odorobo-wait-{}.sock", ulid::Ulid::generate()));
        let console = Console::default();
        console
            .wait_for_socket(socket_path.clone())
            .await
            .expect("start waiting for migration endpoint");
        console.stop_spooling().await;
        assert!(console.spool_task.lock().await.handle.is_none());
        assert!(console.wait_for_socket(socket_path).await.is_err());
    }

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
pub struct MigrationFinished;

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
        let vminstance = VMInstance::spawn(&vmid.to_string(), vm_config_for_ch, boot, None).await?;

        let console = Console::default();
        // A migration receiver has no config yet; its serial socket is created
        // only when the migrated VM is restored.
        if vm_config.is_some() {
            console
                .attach_socket(vminstance.console_socket_path())
                .await?;
        }

        // Poll without holding the process lock across wait: teardown must be
        // able to kill and reap the VMM before releasing its attached resources.
        if let Some(child_process) = vminstance.child_process() {
            let child_process = Arc::downgrade(&child_process);
            let actor_ref = actor_ref.clone();
            tokio::spawn(async move {
                debug!(%vmid, "watching child process to handle actor cleanup");
                let result = loop {
                    let Some(child) = child_process.upgrade() else {
                        return;
                    };
                    let status = child.lock().await.try_wait();
                    drop(child);
                    match status {
                        Ok(None) => {}
                        Ok(Some(status)) => break Ok(status),
                        Err(error) => break Err(error),
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                };
                match result {
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

        self.console.stop_spooling().await;
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

        if let Err(error) = self
            .console
            .wait_for_socket(self.vm_instance.console_socket_path())
            .await
        {
            warn!(?error, "failed to start migration console spool");
        }

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
    type Reply = Result<ShutdownVMReply, String>;
    async fn handle(
        &mut self,
        _msg: ShutdownVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        trace!(vmid = %self.vmid, "Shutting down VM guest while retaining its actor and runtime");
        if let Err(error) = self.vm_instance.shutdown().await {
            error!(vmid = %self.vmid, ?error, "Failed to shut down VM guest");
            return Err(error.to_string());
        }
        Ok(ShutdownVMReply)
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
        trace!(vmid = %self.vmid, "Deleting VM actor and tearing down runtime");
        ctx.actor_ref().stop_gracefully().await.unwrap();
    }
}
