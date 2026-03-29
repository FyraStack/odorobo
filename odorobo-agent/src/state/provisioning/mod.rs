//! Module for provisioning CH instances for odorobo agent.
//!
//! Provides helper functions that calls the necessary hooks and various methods to start a
//! Cloud Hypervisor process for a given instance
mod hooks;
mod systemd;
use stable_eyre::Result;
use tracing::info;

use crate::state::provisioning::systemd::SystemdUnitProvisioner;

pub struct VMProvisioner<B: VMProvisionerBackend> {
    backend: B,
}

impl<B: VMProvisionerBackend> VMProvisioner<B> {
    // todo: call hooks here
    #[tracing::instrument(skip(self))]
    pub async fn start_instance(&self, vmid: &str) -> Result<i32> {
        info!(?vmid, "Starting instance");
        self.backend.start_instance(vmid).await
    }

    #[tracing::instrument(skip(self))]
    pub async fn stop_instance(&self, vmid: &str) -> Result<()> {
        info!(?vmid, "Stopping instance");
        self.backend.stop_instance(vmid).await
    }
}

pub trait VMProvisionerBackend: Send + Sync {
    async fn start_instance(&self, vmid: &str) -> Result<i32>;
    async fn stop_instance(&self, vmid: &str) -> Result<()>;
}

pub fn default_provisioner() -> VMProvisioner<SystemdUnitProvisioner> {
    VMProvisioner {
        backend: SystemdUnitProvisioner,
    }
}
