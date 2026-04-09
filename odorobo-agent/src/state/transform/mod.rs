use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::Result;

pub trait ConfigTransform: Send + Sync {
    fn transform(&self, vmid: &str, config: &mut VmConfig) -> Result<()>;

    /// Optional teardown method to reverse transformations if needed,
    /// used for tearing down VMs
    fn teardown(&self, _vmid: &str, _config: &mut VmConfig) -> Result<()> {
        Ok(())
    }
}

pub mod console;
pub mod path_verify;
pub mod storage;
pub use console::ConsoleTransform;
pub use path_verify::PathVerify;
use tracing::trace;

pub struct TransformChain(Vec<Box<dyn ConfigTransform>>);

impl TransformChain {
    pub fn new() -> Self {
        Self(vec![])
    }

    pub fn add<T: ConfigTransform + 'static>(mut self, transform: T) -> Self {
        self.0.push(Box::new(transform));
        self
    }

    pub fn then(self) -> Box<dyn ConfigTransform> {
        Box::new(self)
    }
}

impl ConfigTransform for TransformChain {
    fn transform(&self, vmid: &str, config: &mut VmConfig) -> Result<()> {
        trace!("Applying transform chain with {} transforms", self.0.len());
        for t in &self.0 {
            t.transform(vmid, config)?;
        }
        Ok(())
    }

    fn teardown(&self, vmid: &str, config: &mut VmConfig) -> Result<()> {
        trace!("Teardown transform chain with {} transforms", self.0.len());
        for t in self.0.iter().rev() {
            t.teardown(vmid, config)?;
        }
        Ok(())
    }
}

impl Default for TransformChain {
    fn default() -> Self {
        Self::new()
            .add(storage::StorageChain::default())
            .add(ConsoleTransform)
            .add(PathVerify)
    }
}
