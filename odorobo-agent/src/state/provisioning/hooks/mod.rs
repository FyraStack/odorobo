//! Hooks for provisioning state.
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
    fn before_start(&self, _vmid: &str) -> HookFuture<'_> {
        Box::pin(async { Ok(()) })
    }
    fn after_start(&self, _vmid: &str, _pid: i32) -> HookFuture<'_> {
        Box::pin(async { Ok(()) })
    }
    fn before_stop(&self, _vmid: &str) -> HookFuture<'_> {
        Box::pin(async { Ok(()) })
    }
    fn after_stop(&self, _vmid: &str) -> HookFuture<'_> {
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

    pub async fn before_start(&self, vmid: &str) -> Result<()> {
        for hook in &self.hooks {
            hook.before_start(vmid).await?;
        }
        Ok(())
    }

    pub async fn after_start(&self, vmid: &str, pid: i32) -> Result<()> {
        for hook in &self.hooks {
            hook.after_start(vmid, pid).await?;
        }
        Ok(())
    }

    pub async fn before_stop(&self, vmid: &str) -> Result<()> {
        for hook in &self.hooks {
            hook.before_stop(vmid).await?;
        }
        Ok(())
    }

    pub async fn after_stop(&self, vmid: &str) -> Result<()> {
        for hook in &self.hooks {
            hook.after_stop(vmid).await?;
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
