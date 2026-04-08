//! Module for provisioning CH instances for odorobo agent.
//!
//! Provides helper functions that calls the necessary hooks and various methods to start a
//! Cloud Hypervisor process for a given instance
mod hooks;
mod systemd;
pub mod actor;
use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::{Result, eyre::Context};
use tracing::info;

use self::hooks::HookManager;
use crate::state::provisioning::systemd::SystemdUnitProvisioner;

pub struct VMProvisioner<B: VMProvisionerBackend> {
    backend: B,
    hooks: HookManager,
}

impl<B: VMProvisionerBackend> VMProvisioner<B> {
    pub fn with_hooks(backend: B, hooks: HookManager) -> Self {
        Self { backend, hooks }
    }

    #[tracing::instrument(skip(self))]
    pub async fn start_instance(&self, vmid: &str, config: &VmConfig) -> Result<i32> {
        info!(?vmid, "Starting instance");
        self.hooks
            .before_start(vmid, config)
            .await
            .wrap_err_with(|| format!("before_start hook failed for VM {vmid}"))?;

        let pid = self
            .backend
            .start_instance(vmid)
            .await
            .wrap_err_with(|| format!("Failed to start VM instance {vmid}"))?;

        self.hooks
            .after_start(vmid, config, pid)
            .await
            .wrap_err_with(|| format!("after_start hook failed for VM {vmid}"))?;

        Ok(pid)
    }

    #[tracing::instrument(skip(self))]
    pub async fn stop_instance(&self, vmid: &str, config: &VmConfig) -> Result<()> {
        info!(?vmid, "Stopping instance");
        self.hooks
            .before_stop(vmid, config)
            .await
            .wrap_err_with(|| format!("before_stop hook failed for VM {vmid}"))?;

        self.backend
            .stop_instance(vmid)
            .await
            .wrap_err_with(|| format!("Failed to stop VM instance {vmid}"))?;

        self.hooks
            .after_stop(vmid, config)
            .await
            .wrap_err_with(|| format!("after_stop hook failed for VM {vmid}"))
    }

    pub async fn before_boot(&self, vmid: &str, config: &VmConfig) -> Result<()> {
        self.hooks
            .before_boot(vmid, config)
            .await
            .wrap_err_with(|| format!("before_start hook failed for VM {vmid}"))
    }

    pub async fn after_boot(&self, vmid: &str, config: &VmConfig) -> Result<()> {
        self.hooks
            .after_boot(vmid, config)
            .await
            .wrap_err_with(|| format!("after_start hook failed for VM {vmid}"))
    }
}

pub trait VMProvisionerBackend: Send + Sync {
    async fn start_instance(&self, vmid: &str) -> Result<i32>;
    async fn stop_instance(&self, vmid: &str) -> Result<()>;
}

pub fn default_provisioner() -> VMProvisioner<SystemdUnitProvisioner> {
    VMProvisioner::with_hooks(SystemdUnitProvisioner, HookManager::default())
}
