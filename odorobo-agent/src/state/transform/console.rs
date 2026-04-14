use cloud_hypervisor_client::models::{ConsoleConfig, VmConfig};
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
            mode: cloud_hypervisor_client::models::ConsoleMode::Off,
            ..Default::default()
        });
        // note: console passthrough is kinda janky and breaks live migration, needs a way to fix this
        //
        // todo: TTY mode also doesn't work well with systemd, need to figure out a good way to
        // remotely attach TTY on boot without breaking systemd or live migration
        //
        // consider some virtual GPU device, but CH doesn't have QXL or virtio-gpu so idk
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

        // TODO: fix vsock support
        // currently it breaks live migration...

        // config.vsock = Some(cloud_hypervisor_client::models::VsockConfig {
        //     cid: 3,
        //     id: Some("odorobo-vsock".into()),
        //     socket: format!("{}/vsock.sock", runtime_path.display()),
        //     ..Default::default()
        // });
        Ok(())
    }
}
