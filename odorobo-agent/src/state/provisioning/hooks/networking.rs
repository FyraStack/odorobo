//! Networking hook for provisioning.
use async_trait::async_trait;
use cloud_hypervisor_client::models::VmInfo;
use kameo::prelude::*;
use stable_eyre::{
    Result,
    eyre::{WrapErr, eyre},
};
use tracing::info;
use ulid::Ulid;

use crate::networking::{
    actor::NetworkAgentActor,
    messages::{AttachTap, DetachTap},
};

use super::ProvisioningHook;

/// Provisioning hook that attaches Cloud Hypervisor TAP devices to the
/// agent-managed bridge after the VM has booted.
pub struct NetworkProvisioningHook;

async fn network_agent() -> Result<ActorRef<NetworkAgentActor>> {
    ActorRef::<NetworkAgentActor>::lookup("network_actor")
        .await
        .map_err(|e| eyre!(e))?
        .ok_or_else(|| eyre!("network actor not found"))
}

fn tap_names(info: &VmInfo) -> Vec<String> {
    info.config
        .net
        .as_ref()
        .map(|nets| {
            nets.iter()
                .filter(|net| net.id.as_deref().is_some_and(|id| id.starts_with("net://")))
                .filter_map(|net| net.tap.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

#[async_trait]
impl ProvisioningHook for NetworkProvisioningHook {
    // Only Odorobo-managed `net://` network IDs participate in bridge attach/detach
    // handling here. Other network devices are left alone.
    async fn after_boot(&self, vmid: &str, config: &VmInfo) -> Result<()> {
        let vmid = Ulid::from_string(vmid)
            .map_err(|err| eyre!("invalid vmid {vmid}: {err}"))
            .wrap_err("failed to parse vmid for networking hook")?;

        let taps = tap_names(config);
        info!(
            vmid = %vmid,
            tap_count = taps.len(),
            "networking after_boot hook invoked for Odorobo-managed net:// TAP devices"
        );
        if taps.is_empty() {
            info!(vmid = %vmid, "no TAP devices present after boot, skipping network attach");
            return Ok(());
        }

        let network_actor = network_agent()
            .await
            .wrap_err("failed to look up network actor")?;

        for tap_name in taps {
            info!(vmid = %vmid, tap = %tap_name, "sending AttachTap to network actor");
            network_actor
                .ask(AttachTap {
                    vmid,
                    tap_name: tap_name.clone(),
                })
                .await
                .map_err(|err| eyre!(err))
                .wrap_err_with(|| format!("failed to send AttachTap for tap {tap_name}"))?;
        }

        Ok(())
    }

    async fn before_stop(&self, vmid: &str, config: &VmInfo) -> Result<()> {
        let vmid = Ulid::from_string(vmid)
            .map_err(|err| eyre!("invalid vmid {vmid}: {err}"))
            .wrap_err("failed to parse vmid for networking hook")?;

        let taps = tap_names(config);
        info!(
            vmid = %vmid,
            tap_count = taps.len(),
            "networking before_stop hook invoked for Odorobo-managed net:// TAP devices"
        );
        if taps.is_empty() {
            info!(vmid = %vmid, "no TAP devices present before stop, skipping network detach");
            return Ok(());
        }

        let network_actor = network_agent()
            .await
            .wrap_err("failed to look up network actor")?;

        for tap_name in taps {
            info!(vmid = %vmid, tap = %tap_name, "sending DetachTap to network actor");
            network_actor
                .ask(DetachTap {
                    vmid,
                    tap_name: tap_name.clone(),
                })
                .await
                .map_err(|err| eyre!(err))
                .wrap_err_with(|| format!("failed to send DetachTap for tap {tap_name}"))?;
        }

        Ok(())
    }
}
