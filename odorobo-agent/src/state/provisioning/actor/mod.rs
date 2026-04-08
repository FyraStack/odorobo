use super::VMProvisionerBackend;
use crate::state::VMInstance;
use crate::state::provisioning::default_provisioner;
use cloud_hypervisor_client::models::VmConfig;
use kameo::prelude::*;
use stable_eyre::Report;
use stable_eyre::Result;
use std::path::PathBuf;
use tracing::error;
use tracing::info;
use tracing::warn;
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
    /// Pre-transform config, transformed config goes into the CH instance itself
    pub vm_config: VmConfig,
    /// path to the Cloud Hypervisor socket, in /run/odorobo/vms/<VMID>/ch.sock
    pub vm_instance: VMInstance,
    // handle to the Cloud Hypervisor process
    process_handle: tokio::process::Child,
}

impl Actor for VMActor {
    // tuple of VM ID and config
    type Args = (ulid::Ulid, VmConfig);
    type Error = Report;

    #[tracing::instrument(skip_all)]
    async fn on_start((vmid, vm_config): Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        let ch_sock_path = VMInstance::runtime_dir_for(&vmid.to_string()).join("ch.sock");

        let vminstance = VMInstance::new(&vmid.to_string(), ch_sock_path.clone());

        tracing::warn!("no-op");
        // spawn CH instance
        // this probably is not an ideal way to do this, but we want a minimal thing
        // so let's spawn CH as a child
        //
        // ...or we go back to that systemd way

        // ownership quirk
        // let value = ch_sock_path.clone();
        let ch_process = tokio::process::Command::new("cloud-hypervisor")
            .arg("--api-socket")
            .arg(&ch_sock_path)
            .spawn()?;
        // tokio::spawn(async move {

        //     Ok::<_, Report>(ch_process)
        // });

        Ok(Self {
            vmid,
            vm_config,
            vm_instance: vminstance,
            process_handle: ch_process,
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

        let res = self.process_handle.wait().await;
        info!(vmid = %self.vmid, ?res, "VM process exited");

        Ok(())
    }
}

// allow conversion from VMActor to VMInstance to call API
impl From<VMActor> for VMInstance {
    fn from(actor: VMActor) -> Self {
        actor.vm_instance
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
