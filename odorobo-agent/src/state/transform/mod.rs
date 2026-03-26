use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::Result;

pub trait ConfigTransform: Send + Sync {
    fn transform(&self, config: &mut VmConfig) -> Result<()>;
}

mod console;
mod path_verify;
pub use console::ConsoleTransform;
pub use path_verify::PathVerify;

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
        .add(PathVerify)
        .then()
        .transform(config)
}
