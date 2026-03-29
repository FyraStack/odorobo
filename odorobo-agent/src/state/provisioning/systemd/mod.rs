pub mod proxy;
use super::VMProvisionerBackend;
use proxy::{start_instance, stop_instance};
use stable_eyre::{Result, eyre::WrapErr};

pub struct SystemdUnitProvisioner;

impl VMProvisionerBackend for SystemdUnitProvisioner {
    async fn start_instance(&self, vmid: &str) -> Result<i32> {
        start_instance(vmid)
            .await
            .wrap_err_with(|| format!("Failed to start instance {vmid} with systemd unit"))
    }

    async fn stop_instance(&self, vmid: &str) -> Result<()> {
        stop_instance(vmid)
            .await
            .wrap_err_with(|| format!("Failed to stop instance {vmid} with systemd unit"))
    }
}
