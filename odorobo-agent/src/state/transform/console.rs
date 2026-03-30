use cloud_hypervisor_client::models::{ConsoleConfig, DebugConsoleConfig, VmConfig};
use stable_eyre::Result;
use tracing::trace;

use crate::state::VMInstance;

use super::ConfigTransform;

#[derive(Debug, Clone)]
pub struct ConsoleTransform;

impl ConfigTransform for ConsoleTransform {
    #[tracing::instrument(skip(config))]
    fn transform(&self, vmid: &str, config: &mut VmConfig) -> Result<()> {
        let runtime_path = VMInstance::runtime_dir_for(vmid);
        trace!("Applying ConsoleTransform");
        config.console = Some(ConsoleConfig {
            mode: cloud_hypervisor_client::models::console_config::Mode::Pty,
            ..Default::default()
        });

        config.debug_console = Some(DebugConsoleConfig {
            mode: cloud_hypervisor_client::models::debug_console_config::Mode::Pty,
            file: Some(format!("{}/debug_console.sock", runtime_path.display())),
            ..Default::default()
        });

        config.vsock = Some(cloud_hypervisor_client::models::VsockConfig {
            cid: 3,
            id: Some("odorobo-vsock".into()),
            socket: format!("{}/vsock.sock", runtime_path.display()),
            ..Default::default()
        });
        Ok(())
    }
}
