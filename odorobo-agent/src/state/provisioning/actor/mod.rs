use crate::state::VMInstance;
use cloud_hypervisor_client::models::VmConfig;
use kameo::prelude::*;
use odorobo_shared::messages::create_vm::{DeleteVM, GetVMInfo, GetVMInfoReply, ShutdownVM};
use stable_eyre::{Report, Result};
use tracing::{error, info, trace, warn};
/*
use std::process::Command;

let output = Command::new("echo")
.arg("Hello world")
.output()
.expect("Failed to execute command");
 */

#[derive(RemoteActor)]
pub struct VMActor {
    pub vmid: ulid::Ulid,
    /// path to the Cloud Hypervisor socket, in /run/odorobo/vms/<VMID>/ch.sock
    pub vm_instance: VMInstance,
}

impl Actor for VMActor {
    // tuple of VM ID and config
    type Args = (ulid::Ulid, VmConfig);
    type Error = Report;

    #[tracing::instrument(skip_all)]
    async fn on_start((vmid, vm_config): Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        // let ch_sock_path = VMInstance::runtime_dir_for(&vmid.to_string()).join("ch.sock");

        // // no transform chain

        // tracing::warn!("no-op");
        // // spawn CH instance
        // // this probably is not an ideal way to do this, but we want a minimal thing
        // // so let's spawn CH as a child
        // //
        // // ...or we go back to that systemd way

        // // ownership quirk
        // // let value = ch_sock_path.clone();
        // let ch_process = tokio::process::Command::new("cloud-hypervisor")
        //     .arg("--api-socket")
        //     .arg(&ch_sock_path)
        //     .spawn()?;
        // tokio::spawn(async move {

        let vminstance = VMInstance::spawn(&vmid.to_string(), Some(vm_config), None).await?;

        Ok(Self {
            vmid,
            vm_instance: vminstance,
        })
    }

    async fn on_stop(
        &mut self,
        actor_ref: WeakActorRef<Self>,
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

// allow conversion from VMActor to VMInstance to call API
impl From<VMActor> for VMInstance {
    fn from(actor: VMActor) -> Self {
        actor.vm_instance
    }
}

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
