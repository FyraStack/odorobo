use cloud_hypervisor_client::models::{ConsoleConfig, VmConfig};
use stable_eyre::Result;
use tracing::trace;

use super::ConfigTransform;

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
