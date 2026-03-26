//! Path verification transformer module
//!
//! Verifies paths for validity, returning errors when an invalid path is found
use std::path::Path;

use cloud_hypervisor_client::models::{ConsoleConfig, VmConfig};
use stable_eyre::{Result, eyre::eyre};
use tracing::trace;

use super::ConfigTransform;

#[derive(Debug, Clone)]
pub struct PathVerify;

impl ConfigTransform for PathVerify {
    #[tracing::instrument(skip(config))]
    fn transform(&self, config: &mut VmConfig) -> Result<()> {
        trace!("Verifying paths");
        let config = config.clone();
        // payload path verification
        if let Some(kernel_path) = config.payload.kernel {
            if !Path::new(&kernel_path).is_absolute() {
                return Err(eyre!("Kernel must be an absolute path"));
            }
        }
        if let Some(initramfs_path) = config.payload.initramfs {
            if !Path::new(&initramfs_path).is_absolute() {
                return Err(eyre!("initramfs must be an absolute path"));
            }
        }
        if let Some(firmware_path) = config.payload.firmware {
            if !Path::new(&firmware_path).is_absolute() {
                return Err(eyre!("firmware must be an absolute path"));
            }
        }

        // storage path verification
        if let Some(disk_configs) = config.disks {
            for disk in disk_configs {
                if let Some(path) = disk.path {
                    let disk_id = disk.id.unwrap_or("<unknown>".into());
                    if !Path::new(&path).is_absolute() {
                        return Err(eyre!("disk path for {disk_id} must be an absolute path"));
                    }
                }
            }
        }
        Ok(())
    }
}
