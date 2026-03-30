//! Hooks for provisioning state.
//!
//! These are hooks, code that runs at various points during the provisioning lifecycle, that can be used to perform additional actions related to provisioning
//!
//! For example, networking setup, registering with systemd-machined, etc.
//!
//! They are different from transforms which are designed to modify the configuration itself
//! to accomodate for the host environment, while hooks provide ways for the host itself
//! to react to provisioning events and perform necessary setup/teardown actions.
use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::Result;
use std::{future::Future, pin::Pin};

mod machined;

// hack: our own mini async-trait implementation
// because for some reason using async inside a dynamic dispatch trait
// like this causes the compiler to complain about
// not being able to make vtables for the trait, even though
// rust 2024 should have been supporting this...
//
// and I don't want to pull in the async-trait crate just for this
pub type HookFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

pub trait ProvisioningHook: Send + Sync {
    fn before_start(&self, _vmid: &str, config: &VmConfig) -> HookFuture<'_> {
        Box::pin(async { Ok(()) })
    }
    fn after_start(&self, _vmid: &str, _config: &VmConfig, _pid: i32) -> HookFuture<'_> {
        Box::pin(async { Ok(()) })
    }
    fn before_stop(&self, _vmid: &str, _config: &VmConfig) -> HookFuture<'_> {
        Box::pin(async { Ok(()) })
    }
    fn after_stop(&self, _vmid: &str, _config: &VmConfig) -> HookFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn before_boot(&self, _vmid: &str, _config: &VmConfig) -> HookFuture<'_> {
        Box::pin(async { Ok(()) })
    }
    fn after_boot(&self, _vmid: &str, _config: &VmConfig) -> HookFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    // fn before_destroy(&self, _vmid: &str) -> HookFuture<'_> {
    //     Box::pin(async { Ok(()) })
    // }
    // fn after_destroy(&self, _vmid: &str) -> HookFuture<'_> {
    //     Box::pin(async { Ok(()) })
    // }
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
