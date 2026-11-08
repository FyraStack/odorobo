//! Networking hook for provisioning.
use async_trait::async_trait;
use cloud_hypervisor_client::models::VmInfo;
use kameo::prelude::*;
use stable_eyre::{
    Result,
    eyre::{WrapErr, eyre},
};
use tracing::{info, warn};
use ulid::Ulid;

use crate::networking::{
    actor::NetworkAgentActor,
    messages::{DetachNic, UnpublishPorts},
};

use super::ProvisioningHook;

/// Provisioning hook that releases network resources before a VM stops.
///
/// TAP attachment happens in the VM actor: Cloud Hypervisor creates the TAP
/// while booting and the agent asks the network actor to configure it right
/// after boot, before the guest can transmit.
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
    // Only Odorobo-managed `net://` network IDs participate in NIC detach
    // handling here. Other network devices are left alone.
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
        let network_actor = match network_agent().await {
            Ok(actor) => actor,
            Err(err) if taps.is_empty() => {
                // A stop must not hang on a network actor that is not there;
                // there is nothing to detach and no ports left to unpublish.
                warn!(%vmid, ?err, "no network actor during stop; skipping port unpublish");
                return Ok(());
            }
            Err(err) => return Err(err.wrap_err("failed to look up network actor")),
        };

        // Remove published ports even if the TAP has already disappeared.
        if let Err(err) = network_actor.ask(UnpublishPorts { vmid }).await {
            warn!(%vmid, ?err, "failed to remove published ports");
        }

        if taps.is_empty() {
            info!(vmid = %vmid, "no TAP devices present before stop, skipping NIC detach");
            return Ok(());
        }

        for tap_name in taps {
            info!(vmid = %vmid, tap = %tap_name, "sending DetachNic to network actor");
            network_actor
                .ask(DetachNic {
                    vmid,
                    tap_name: tap_name.clone(),
                })
                .await
                .map_err(|err| eyre!(err))
                .wrap_err_with(|| format!("failed to send DetachNic for tap {tap_name}"))?;
        }

        Ok(())
    }
}
