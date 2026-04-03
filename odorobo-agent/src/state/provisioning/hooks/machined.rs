//! systemd-machined integration hook for odorobo agent

use crate::state::VMInstance;
use crate::state::provisioning::hooks::{HookFuture, ProvisioningHook};
use crate::util::zbus_system_connection;
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

impl ProvisioningHook for CHMachineProvisioningHook {
    fn after_start(&self, vmid: &str, _config: &VmConfig, pid: i32) -> HookFuture<'_> {
        let vmid = vmid.to_string();
        Box::pin(async move {
            tracing::info!(vmid, pid, "Registering machine with systemd-machined");
            let runtime_dir = VMInstance::runtime_dir_for(&vmid);
            let manager = get_manager_proxy().await?;
            let res = manager
                .register_machine(
                    vmid.clone(),
                    Vec::new(),
                    SERVICE_CLASS.to_string(),
                    "vm".to_string(),
                    pid as u32,
                    runtime_dir.display().to_string(),
                )
                .await
                .wrap_err("Failed to register machine with systemd-machined");

            if let Err(e) = res {
                tracing::error!(vmid, error = ?e, "Failed to register machine with systemd-machined");
                tracing::warn!(vmid, "Continuing without systemd-machined registration");
            }

            Ok(())
        })
    }

    fn before_stop(&self, vmid: &str, _config: &VmConfig) -> HookFuture<'_> {
        let vmid = vmid.to_string();
        Box::pin(async move {
            tracing::info!(vmid, "Unregistering machine from systemd-machined");
            let manager = get_manager_proxy().await?;
            let res = manager
                .unregister_machine(vmid.clone())
                .await
                .wrap_err("Failed to unregister machine from systemd-machined");

            // don't fail if hook fails, just log the error
            if let Err(e) = res {
                tracing::error!(vmid, error = ?e, "Failed to unregister machine from systemd-machined");
            }

            Ok(())
        })
    }
}
