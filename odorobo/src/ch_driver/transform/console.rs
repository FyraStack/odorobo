use cloud_hypervisor_client::models::{ConsoleConfig, VmConfig};
use stable_eyre::Result;
use tracing::trace;

use crate::ch_driver::VMInstance;

use super::ConfigTransform;

#[derive(Debug, Clone)]
pub struct ConsoleTransform;

impl ConfigTransform for ConsoleTransform {
    #[tracing::instrument(skip(config))]
    fn transform(&self, vmid: &str, config: &mut VmConfig) -> Result<()> {
        let runtime_path = VMInstance::runtime_dir_for(vmid);
        trace!("Applying ConsoleTransform");
        config.console = Some(ConsoleConfig {
            mode: cloud_hypervisor_client::models::ConsoleMode::Off,
            ..Default::default()
        });
        // Use a Unix socket serial console: TTY passthrough is incompatible with
        // systemd and live migration. A graphical console is unavailable because
        // Cloud Hypervisor does not currently provide QXL or virtio-gpu support.
        config.serial = Some(ConsoleConfig {
            mode: cloud_hypervisor_client::models::ConsoleMode::Socket,
            // file: Some(format!("{}/serial", runtime_path.display())),
            socket: Some(format!("{}/console.sock", runtime_path.display())),
            ..Default::default()
        });

        // config.debug_console = Some(DebugConsoleConfig {
        //     mode: cloud_hypervisor_client::models::debug_console_config::Mode::Pty,
        //     file: Some(format!("{}/debug_console.sock", runtime_path.display())),
        //     ..Default::default()
        // });

        Ok(())
    }
}
