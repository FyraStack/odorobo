use cloud_hypervisor_client::models::{ConsoleConfig, VmConfig};
use stable_eyre::Result;
use tracing::trace;

pub trait ConfigTransform: Send + Sync {
    fn transform(&self, config: &mut VmConfig) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct ConsoleTransform;

impl ConfigTransform for ConsoleTransform {
    #[tracing::instrument(skip(config))]
    fn transform(&self, config: &mut VmConfig) -> Result<()> {
        trace!("Applying ConsoleTransform");
        config.console = Some(ConsoleConfig {
            mode: cloud_hypervisor_client::models::console_config::Mode::Pty,
            ..Default::default()
        });
        Ok(())
    }
}

#[derive(Default)]
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
    fn transform(&self, config: &mut VmConfig) -> Result<()> {
        for t in &self.0 {
            t.transform(config)?;
        }
        Ok(())
    }
}

pub fn apply_builtin_transforms(config: &mut VmConfig) -> Result<()> {
    TransformChain::new()
        .add(ConsoleTransform)
        .then()
        .transform(config)
}
