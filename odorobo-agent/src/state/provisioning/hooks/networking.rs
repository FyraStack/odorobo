//! Networking hook for provisioning.
use async_trait::async_trait;
use cloud_hypervisor_client::models::VmInfo;
use kameo::prelude::*;
use stable_eyre::{
    Result,
    eyre::{WrapErr, eyre},
};
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
                .filter_map(|net| net.tap.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

#[async_trait]
impl ProvisioningHook for NetworkProvisioningHook {
    // CH auto-generates TAP names for each network device and create them if they don't
    // already exist, so we are okay with the tap names we get from the VM config
    async fn after_boot(&self, vmid: &str, config: &VmInfo) -> Result<()> {
        let vmid = Ulid::from_string(vmid)
            .map_err(|err| eyre!("invalid vmid {vmid}: {err}"))
            .wrap_err("failed to parse vmid for networking hook")?;

        let taps = tap_names(config);
        if taps.is_empty() {
            return Ok(());
        }

        let network_actor = network_agent()
            .await
            .wrap_err("failed to look up network actor")?;

        for tap_name in taps {
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
        if taps.is_empty() {
            return Ok(());
        }

        let network_actor = network_agent()
            .await
            .wrap_err("failed to look up network actor")?;

        for tap_name in taps {
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
