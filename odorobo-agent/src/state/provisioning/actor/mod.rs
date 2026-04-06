use super::VMProvisionerBackend;
use crate::state::VMInstance;
use cloud_hypervisor_client::models::VmConfig;
use kameo::prelude::*;
use stable_eyre::Report;
use stable_eyre::{Result};
use std::path::PathBuf; /*
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
}

impl Actor for VMActor {
    type Args = Self;
    type Error = Report;

    #[tracing::instrument(skip_all)]
    async fn on_start(state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        
        tracing::warn!("no-op");
        Ok(state)
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

/// Provisioner backend for VM instances using an actor-based model
pub struct ActorProvisioner;

impl VMProvisionerBackend for ActorProvisioner {
    async fn start_instance(&self, vmid: &str) -> Result<i32> {
        todo!()
    }

    async fn stop_instance(&self, vmid: &str) -> Result<()> {
        todo!()
    }
}
