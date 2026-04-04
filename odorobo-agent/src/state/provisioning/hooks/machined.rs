//! systemd-machined integration hook for odorobo agent

use crate::state::VMInstance;
use crate::state::provisioning::hooks::ProvisioningHook;
use crate::util::zbus_system_connection;
use async_trait::async_trait;
use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::eyre::Context;
use stable_eyre::{Result, eyre::eyre};
use zbus_systemd::machine1::ManagerProxy;

async fn get_manager_proxy() -> Result<ManagerProxy<'static>> {
    let connection = zbus_system_connection().await?;
    ManagerProxy::new(&connection)
        .await
        .wrap_err("Failed to create systemd manager proxy")
}

pub struct CHMachine {
    vmid: String,
}

impl TryFrom<CHMachine> for VMInstance {
    type Error = stable_eyre::Report;

    fn try_from(value: CHMachine) -> Result<Self, Self::Error> {
        VMInstance::get(&value.vmid).ok_or_else(|| {
            eyre!(
                "Failed to get VMInstance for CHMachine with vmid {}",
                value.vmid
            )
            .wrap_err("Failed to convert CHMachine to VMInstance")
        })
    }
}

pub const SERVICE_CLASS: &str = "cloud-hypervisor";

pub struct CHMachineProvisioningHook;

#[async_trait]
impl ProvisioningHook for CHMachineProvisioningHook {
    async fn after_start(&self, vmid: &str, _config: &VmConfig, pid: i32) -> Result<()> {
        if pid == 0 {
            tracing::warn!(
                vmid,
                "Skipping systemd-machined registration: PID is 0 (service not yet active)"
            );
            return Ok(());
        }
        tracing::info!(vmid, pid, "Registering machine with systemd-machined");
        let runtime_dir = VMInstance::runtime_dir_for(vmid);
        let res = async {
            let manager = get_manager_proxy().await?;
            manager
                .register_machine(
                    vmid.to_string(),
                    Vec::new(),
                    SERVICE_CLASS.to_string(),
                    "vm".to_string(),
                    pid as u32,
                    runtime_dir.display().to_string(),
                )
                .await
                .wrap_err("Failed to register machine with systemd-machined")
        }
        .await;

        if let Err(e) = res {
            tracing::error!(vmid, error = ?e, "Failed to register machine with systemd-machined");
            tracing::warn!(vmid, "Continuing without systemd-machined registration");
        }

        Ok(())
    }

    async fn before_stop(&self, vmid: &str, _config: &VmConfig) -> Result<()> {
        tracing::info!(vmid, "Unregistering machine from systemd-machined");
        let res = async {
            let manager = get_manager_proxy().await?;
            manager
                .unregister_machine(vmid.to_string())
                .await
                .wrap_err("Failed to unregister machine from systemd-machined")
        }
        .await;

        if let Err(e) = res {
            tracing::error!(vmid, error = ?e, "Failed to unregister machine from systemd-machined");
        }

        Ok(())
    }
}
