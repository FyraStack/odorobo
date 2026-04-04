//! Hooks for provisioning state.
//!
//! These are hooks, code that runs at various points during the provisioning lifecycle, that can be used to perform additional actions related to provisioning
//!
//! For example, networking setup, registering with systemd-machined, etc.
//!
//! They are different from transforms which are designed to modify the configuration itself
//! to accomodate for the host environment, while hooks provide ways for the host itself
//! to react to provisioning events and perform necessary setup/teardown actions.
use async_trait::async_trait;
use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::Result;

mod machined;


// Rust 1.75 does not support dyn async traits, we still need async_trait for this
#[async_trait]
pub trait ProvisioningHook: Send + Sync {
    async fn before_start(&self, _vmid: &str, _config: &VmConfig) -> Result<()> {
        Ok(())
    }
    async fn after_start(&self, _vmid: &str, _config: &VmConfig, _pid: i32) -> Result<()> {
        Ok(())
    }
    async fn before_stop(&self, _vmid: &str, _config: &VmConfig) -> Result<()> {
        Ok(())
    }
    async fn after_stop(&self, _vmid: &str, _config: &VmConfig) -> Result<()> {
        Ok(())
    }
    async fn before_boot(&self, _vmid: &str, _config: &VmConfig) -> Result<()> {
        Ok(())
    }
    async fn after_boot(&self, _vmid: &str, _config: &VmConfig) -> Result<()> {
        Ok(())
    }
}

pub struct HookManager {
    hooks: Vec<Box<dyn ProvisioningHook>>,
}

impl HookManager {
    pub fn add_hook<T: ProvisioningHook + 'static>(mut self, hook: T) -> Self {
        self.hooks.push(Box::new(hook));
        self
    }

    pub async fn before_start(&self, vmid: &str, config: &VmConfig) -> Result<()> {
        for hook in &self.hooks {
            hook.before_start(vmid, config).await?;
        }
        Ok(())
    }

    pub async fn after_start(&self, vmid: &str, config: &VmConfig, pid: i32) -> Result<()> {
        for hook in &self.hooks {
            hook.after_start(vmid, config, pid).await?;
        }
        Ok(())
    }

    pub async fn before_stop(&self, vmid: &str, config: &VmConfig) -> Result<()> {
        for hook in &self.hooks {
            hook.before_stop(vmid, config).await?;
        }
        Ok(())
    }

    pub async fn after_stop(&self, vmid: &str, config: &VmConfig) -> Result<()> {
        for hook in &self.hooks {
            hook.after_stop(vmid, config).await?;
        }
        Ok(())
    }

    pub async fn before_boot(&self, vmid: &str, config: &VmConfig) -> Result<()> {
        for hook in &self.hooks {
            hook.before_boot(vmid, config).await?;
        }
        Ok(())
    }

    pub async fn after_boot(&self, vmid: &str, config: &VmConfig) -> Result<()> {
        for hook in &self.hooks {
            hook.after_boot(vmid, config).await?;
        }
        Ok(())
    }
}

impl Default for HookManager {
    fn default() -> Self {
        Self {
            hooks: vec![Box::new(machined::CHMachineProvisioningHook)],
        }
    }
}
