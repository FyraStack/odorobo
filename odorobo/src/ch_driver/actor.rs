use crate::{ch_driver::VMInstance, types::VirtualMachine};
use cloud_hypervisor_client::models::{CpuFeatures, CpusConfig, DiskConfig, ImageType, MemoryConfig, NetConfig, PayloadConfig, PlatformConfig, VmConfig};
use kameo::prelude::*;
use crate::messages::vm::{
    DeleteVM, GetVMInfo, GetVMInfoReply, MigrateVMReceive, MigrateVMReceiveReply, PrepMigration,
    ShutdownVM,
};
use serde::{Deserialize, Serialize};
use stable_eyre::{Report, Result};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace, warn};

/// A migration state that holds the listening address and VM config for a migration,
/// used to pass live migration data between actors.
pub struct MigrationState {
    pub listening_address: String,
    pub config: VmConfig,
    /// The task handle for the migration process.
    pub migration_task: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationFinished;

#[derive(RemoteActor)]
pub struct VMActor {
    pub vmid: ulid::Ulid,
    /// path to the Cloud Hypervisor socket, in /run/odorobo/vms/<VMID>/ch.sock
    pub vm_instance: VMInstance,
    pub migration_state: Option<MigrationState>,
}

impl Actor for VMActor {
    // tuple of VM ID and optional config
    type Args = (ulid::Ulid, Option<VirtualMachine>);
    type Error = Report;

    #[tracing::instrument(skip_all)]
    async fn on_start((vmid, vm_config): Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        let mut vminstance = VMInstance::spawn(&vmid.to_string(), vm_config.map(VmConfig::from), None).await?;

        // Take the child process out so we can watch for unexpected death.
        // destroy() handles a missing child_process gracefully.
        if let Some(mut child_process) = vminstance.take_child_process() {
            let actor_ref = actor_ref.clone();
            tokio::spawn(async move {
                debug!(%vmid, "watching child process to handle actor cleanup");
                match child_process.wait().await {
                    Ok(status) => {
                        if !status.success() {
                            error!(%vmid, ?status, "child process exited unexpectedly, killing actor");
                            let _ = actor_ref.kill();
                        } else {
                            warn!(%vmid, "child process exited outside of actor teardown");
                            let _ = actor_ref.stop_gracefully().await;
                        }
                    }
                    Err(err) => {
                        error!(%vmid, ?err, "failed to wait on child process, killing actor");
                        actor_ref.kill();
                    }
                };
            });
        } else {
            warn!(%vmid, "VMInstance has no child process to watch");
        }

        Ok(Self {
            vmid,
            vm_instance: vminstance,
            migration_state: None,
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
        VmConfig {
            cpus: Some(CpusConfig {
                boot_vcpus: vm.data.vcpus as i32,
                max_vcpus: vm.data.max_vcpus.unwrap_or(vm.data.vcpus) as i32,
                kvm_hyperv: Some(false),
                nested: Some(false),
                features: Some(CpuFeatures {
                    amx: Some(false)
                }),
                ..Default::default()
            }),
            memory: Some(MemoryConfig {
                size: vm.data.memory.as_u64() as i64,
                mergeable: Some(false),
                hotplug_method: Some("Acpi".to_string()),
                shared: Some(true),
                hugepages: Some(false),
                prefault: Some(false),
                thp: Some(true),
                ..Default::default()
            }),
            payload: PayloadConfig {
                firmware: Some("/var/lib/odorobo/CLOUDHV.fd".to_string()),
                ..Default::default()
            },
            disks: Some(vec![
                DiskConfig { // todo: get cappy to make this auto generate this via the manifest's volumes atribute.
                    // todo: the json i was given by cappy had disable_io_uring and disable_aio in this config, but I can't find these. I assume they were just a mistake.
                    path: Some(vm.data.image),
                    readonly: Some(false),
                    direct: Some(false),
                    iommu: Some(false),
                    num_queues: Some(1),
                    queue_size: Some(128),
                    vhost_user: Some(false),
                    id: Some("_disk0".to_string()),
                    pci_segment: Some(0),
                    backing_files: Some(false),
                    sparse: Some(true),
                    image_type: Some(ImageType::Raw),
                    ..Default::default()
                }
            ]),
            net: Some(vec![
                NetConfig {
                    id: Some("net://devnet".to_string()),
                    mac: Some("46:59:52:67:67:67".to_string()),
                    ..Default::default()
                }
            ]),
            platform: Some(PlatformConfig {
                serial_number: Some("ds=nocloud".to_string()),
                ..Default::default()
            }),
            landlock_enable: Some(false),
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
impl Message<GetVMInfo> for VMActor {
    type Reply = GetVMInfoReply;
    async fn handle(
        &mut self,
        _msg: GetVMInfo,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        GetVMInfoReply {
            vmid: self.vmid,
            config: self.vm_instance.vm_config.clone(),
        }
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
