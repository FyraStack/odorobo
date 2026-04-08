use super::VMProvisionerBackend;
use crate::state::VMInstance;
use cloud_hypervisor_client::models::VmConfig;
use kameo::prelude::*;
use stable_eyre::Report;
use stable_eyre::Result;
use std::path::PathBuf;
use crate::state::provisioning::default_provisioner;
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
    pub ch_socket_path: PathBuf,
    // handle to the Cloud Hypervisor process
    // process_handle: tokio::process::Child,
}

impl Actor for VMActor {
    // tuple of VM ID and config
    type Args = (ulid::Ulid, VmConfig);
    type Error = Report;

    #[tracing::instrument(skip_all)]
    async fn on_start((vmid, vm_config): Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        let ch_sock_path = VMInstance::runtime_dir_for(&vmid.to_string()).join("ch.sock");

        tracing::warn!("no-op");
        // spawn CH instance
        // this probably is not an ideal way to do this, but we want a minimal thing
        // so let's spawn CH as a child
        //
        // ...or we go back to that systemd way
        // let ch_process = tokio::process::Command::new("cloud-hypervisor")
        //     .arg("--api-socket")
        //     .arg(&ch_sock_path);

        // use the provisioner to spawn the VM instance
        // consider spawning transient services instead for easier code deployment
        default_provisioner().start_instance(&vmid.to_string(), &vm_config).await?;

        Ok(Self {
            vmid,
            vm_config,
            ch_socket_path: ch_sock_path,
            // process_handle: ch_process.spawn()?,
        })
    }
}

// allow conversion from VMActor to VMInstance to call API
impl From<VMActor> for VMInstance {
    fn from(actor: VMActor) -> Self {
        Self {
            id: actor.vmid.into(),
            ch_socket_path: actor.ch_socket_path,
        }
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
