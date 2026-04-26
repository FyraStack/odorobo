use crate::state::VMInstance;
use cloud_hypervisor_client::{
    apis::DefaultApi,
    models::{DeviceConfig, NetConfig, PciDeviceInfo, VmRemoveDevice},
};
use stable_eyre::{Result, eyre::Context};

impl VMInstance {
    pub async fn add_device(&self, device_config: DeviceConfig) -> Result<PciDeviceInfo> {
        self.conn()
            .vm_add_device_put(device_config)
            .await
            .wrap_err_with(|| format!("Failed to add device to VM {}", self.id))
    }

    pub async fn remove_device(&self, remove_device: VmRemoveDevice) -> Result<()> {
        self.conn()
            .vm_remove_device_put(remove_device)
            .await
            .wrap_err_with(|| format!("Failed to remove device from VM {}", self.id))
    }

    pub async fn add_net(&self, net_config: NetConfig) -> Result<PciDeviceInfo> {
        self.conn()
            .vm_add_net_put(net_config)
            .await
            .wrap_err_with(|| format!("Failed to add network device to VM {}", self.id))
    }
}
