use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::{Result, eyre::eyre};

pub trait ConfigTransform: Send + Sync {
    fn transform(&self, vmid: &str, config: &mut VmConfig) -> Result<()>;

    /// Optional teardown method to reverse transformations if needed,
    /// used for tearing down VMs
    fn teardown(&self, _vmid: &str, _config: &mut VmConfig) -> Result<()> {
        Ok(())
    }
}

pub mod console;
pub mod networking;
pub mod path_verify;
pub mod storage;
pub use console::ConsoleTransform;
pub use networking::NetworkTransform;
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
        let mut errors = Vec::new();
        for t in self.0.iter().rev() {
            if let Err(error) = t.teardown(vmid, config) {
                errors.push(format!("{error:#}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(eyre!(
                "Failed to tear down transforms: {}",
                errors.join("; ")
            ))
        }
    }
}

impl Default for TransformChain {
    fn default() -> Self {
        Self::new()
            .add(storage::StorageDriverTransformer::default())
            .add(ConsoleTransform)
            .add(NetworkTransform)
            .add(PathVerify)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct FailingTeardown {
        calls: Arc<Mutex<Vec<usize>>>,
        index: usize,
    }

    impl ConfigTransform for FailingTeardown {
        fn transform(&self, _vmid: &str, _config: &mut VmConfig) -> Result<()> {
            Ok(())
        }

        fn teardown(&self, _vmid: &str, _config: &mut VmConfig) -> Result<()> {
            self.calls
                .lock()
                .map_err(|error| eyre!("{error}"))?
                .push(self.index);
            Err(eyre!("failure {}", self.index))
        }
    }

    #[test]
    fn teardown_continues_in_reverse_order_and_reports_all_errors() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let chain = TransformChain::new()
            .add(FailingTeardown {
                calls: Arc::clone(&calls),
                index: 0,
            })
            .add(FailingTeardown {
                calls: Arc::clone(&calls),
                index: 1,
            });
        let error = chain.teardown("vm", &mut VmConfig::default()).unwrap_err();
        assert_eq!(*calls.lock().unwrap(), [1, 0]);
        assert!(error.to_string().contains("failure 0"));
        assert!(error.to_string().contains("failure 1"));
    }
}
