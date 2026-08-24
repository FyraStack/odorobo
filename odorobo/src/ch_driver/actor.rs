use std::{collections::VecDeque, sync::Arc};

use crate::messages::vm::{
    DeleteVM, GetConsoleHistory, GetConsoleHistoryReply, GetVMHeartbeat, GetVMHeartbeatReply,
    GetVMInfo, GetVMInfoReply, MigrateVMReceive, MigrateVMReceiveReply, PrepMigration,
    SendConsoleInput, SendConsoleInputReply, ShutdownVM,
};
use crate::{ch_driver::VMInstance, types::VirtualMachine};
use cloud_hypervisor_client::models::{
    CpusConfig, DiskConfig, ImageType, MemoryConfig, PayloadConfig, PlatformConfig, VmConfig,
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

/// A migration state that holds the listening address and VM config for a migration,
/// used to pass live migration data between actors.
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
    pub async fn attach(socket_path: std::path::PathBuf) -> Result<Self> {
        let stream = UnixStream::connect(&socket_path).await.map_err(|err| {
            Report::msg(format!(
                "failed to attach console spool to {}: {err}",
                socket_path.display()
            ))
        })?;
        let (mut reader, writer) = stream.into_split();
        let (output, _) = broadcast::channel(256);
        let console = Self {
            inner: Arc::new(Mutex::new(ConsoleBuffer::default())),
            output,
            writer: Arc::new(Mutex::new(Some(writer))),
        };
        let spool = console.clone();
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
        Ok(console)
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
pub struct MigrationFinished;

#[derive(RemoteActor)]
pub struct VMActor {
    pub vmid: ulid::Ulid,
    /// path to the Cloud Hypervisor socket, in /run/odorobo/vms/<VMID>/ch.sock
    pub vm_instance: VMInstance,
    pub migration_state: Option<MigrationState>,
    pub console: Console,
    pub manifest: Option<VirtualMachine>,
}

impl Actor for VMActor {
    // tuple of VM ID and manifest
    type Args = (ulid::Ulid, Option<VirtualMachine>);
    type Error = Report;

    #[tracing::instrument(skip_all)]
    async fn on_start((vmid, vm_config): Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        let mut vminstance = VMInstance::spawn(
            &vmid.to_string(),
            vm_config.clone().map(VmConfig::from),
            None,
        )
        .await?;

        // attach console on startup for spooling
        let console = Console::attach(vminstance.console_socket_path()).await?;

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

        self.vm_instance.destroy().await?;

        // info!(vmid = %self.vmid, ?res, "VM process exited");

        Ok(())
    }
}

// todo: improve a lot of these config options. most of them should be set by the manifest
impl From<VirtualMachine> for VmConfig {
    fn from(vm: VirtualMachine) -> Self {
        Self {
            cpus: Some(CpusConfig {
                boot_vcpus: vm.data.vcpus.cast_signed(),
                max_vcpus: vm.data.max_vcpus.unwrap_or(vm.data.vcpus).cast_signed(),
                ..Default::default()
            }),
            memory: Some(MemoryConfig {
                size: vm.data.memory.as_u64().cast_signed(),
                ..Default::default()
            }),
            payload: PayloadConfig {
                firmware: Some("/var/lib/odorobo/CLOUDHV.fd".to_owned()),
                ..Default::default()
            },
            disks: Some(vec![DiskConfig {
                // todo: get cappy to make this auto generate this via the manifest's volumes atribute.
                path: Some(vm.data.image),
                image_type: Some(ImageType::Raw),
                ..Default::default()
            }]),
            // todo: generate from VM network field
            // net: Some(vec![
            //     NetConfig {
            //         id: Some("net://devnet".to_string()),
            //         ..Default::default()
            //     }
            // ]),
            platform: Some(PlatformConfig {
                serial_number: Some("ds=nocloud".to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        }
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
            config: self.manifest.clone(), // we likely dont want to send the entire manifest on every update, but some of this data is required and this is easier for now.
        }
    }
}

#[remote_message]
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
            };
        }

        let prep_config = msg.config.clone();

        // Start receiving migration on the destination VM (this actor)
        // todo: handle unwrap properly
        let (listening_address, migration_task) = self
            .vm_instance
            .receive_migration()
            .await
            .expect("sending migration request failed");

        // create ongoing migration state
        self.migration_state = Some(MigrationState {
            migration_task: Some(migration_task),
            listening_address: listening_address.clone(),
            config: msg.config,
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

        MigrateVMReceiveReply { listening_address }
    }
}

#[remote_message]
impl Message<MigrationFinished> for VMActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: MigrationFinished,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if self.migration_state.take().is_some() {
            // todo: post-migration cleanup
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
        // todo: prepare devices, volumes, and apply migrated config
        self.vm_instance.prep_config(msg.config).await.unwrap();
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

// /// Provisioner backend for VM instances using an actor-based model
// pub struct ActorProvisioner;

// impl VMProvisionerBackend for ActorProvisioner {
//     async fn start_instance(&self, vmid: &str) -> Result<i32> {
//         todo!()
//     }

//     async fn stop_instance(&self, vmid: &str) -> Result<()> {
//         todo!()
//     }
// }
