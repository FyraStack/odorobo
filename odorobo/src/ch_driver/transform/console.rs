use cloud_hypervisor_client::models::{ConsoleConfig, VmConfig};
use stable_eyre::Result;
use tracing::trace;

use crate::ch_driver::{CONSOLE_SOCKET_FILE_NAME, VMInstance, WORKLOAD_CONSOLE_SOCKET_FILE_NAME};

use super::ConfigTransform;

/// Which guest consoles a VM gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLayout {
    /// One console on the 16550 UART, the only console a disk VM's own kernel
    /// is likely to use.
    SerialOnly,
    /// Kernel and init logs on the serial console, the workload's
    /// stdin/stdout/stderr on the virtio console.
    SplitWorkloadConsole,
    /// Everything on the virtio console, with no serial console at all.
    VirtioOnly,
}

#[derive(Debug, Clone, Copy)]
pub struct ConsoleTransform {
    layout: ConsoleLayout,
}

impl ConsoleTransform {
    pub const fn serial_only() -> Self {
        Self {
            layout: ConsoleLayout::SerialOnly,
        }
    }

    pub const fn split_workload_console() -> Self {
        Self {
            layout: ConsoleLayout::SplitWorkloadConsole,
        }
    }

    pub const fn virtio_only() -> Self {
        Self {
            layout: ConsoleLayout::VirtioOnly,
        }
    }
}

impl Default for ConsoleTransform {
    fn default() -> Self {
        Self::serial_only()
    }
}

impl ConfigTransform for ConsoleTransform {
    #[tracing::instrument(skip(config))]
    fn transform(&self, vmid: &str, config: &mut VmConfig) -> Result<()> {
        let runtime_path = VMInstance::runtime_dir_for(vmid);
        trace!(?self.layout, "Applying ConsoleTransform");
        let off = ConsoleConfig {
            mode: cloud_hypervisor_client::models::ConsoleMode::Off,
            ..Default::default()
        };
        let socket = |name: &str| ConsoleConfig {
            mode: cloud_hypervisor_client::models::ConsoleMode::Socket,
            socket: Some(format!("{}/{name}", runtime_path.display())),
            ..Default::default()
        };
        match self.layout {
            ConsoleLayout::SerialOnly => {
                config.console = Some(off);
                config.serial = Some(socket(CONSOLE_SOCKET_FILE_NAME));
            }
            ConsoleLayout::SplitWorkloadConsole => {
                config.serial = Some(socket(CONSOLE_SOCKET_FILE_NAME));
                config.console = Some(socket(WORKLOAD_CONSOLE_SOCKET_FILE_NAME));
            }
            ConsoleLayout::VirtioOnly => {
                config.serial = Some(off);
                config.console = Some(socket(CONSOLE_SOCKET_FILE_NAME));
            }
        }
        Ok(())
    }
}
