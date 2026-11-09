//! EVPN fabric membership: this node's VTEP and the FRR configuration that advertises its VMs.
//!
//! FRR is driven by vtysh deltas only; `frr.conf` stays a boot-time cache.

use std::net::Ipv4Addr;
use std::time::Duration;

use ipnet::Ipv4Net;
use stable_eyre::Report;
use stable_eyre::eyre::{Context as EyreContext, eyre};
use tracing::debug;

/// Route-map and prefix-list that let only per-VM `/32`s into BGP, never the unreachable default.
const VM_ROUTES: &str = "odorobo-vm-routes";
/// BGP peer group every fabric neighbor joins.
const PEER_GROUP: &str = "fabric";
/// vtysh's reply when removing a BGP instance that is not configured.
const MISSING_BGP_INSTANCE: &str = "Can't find BGP instance";
/// Upper bound for one vtysh run, so a stalled FRR daemon cannot block the network actor.
const VTYSH_TIMEOUT: Duration = Duration::from_secs(30);

/// A failed vtysh command, carrying vtysh's own message.
#[derive(Debug, thiserror::Error)]
#[error("vtysh: {0}")]
struct VtyshError(String);

pub struct Fabric {
    pub vtep: Ipv4Addr,
    underlay: Ipv4Net,
    asn: u32,
}

impl Fabric {
    pub const fn new(vtep: Ipv4Addr, underlay: Ipv4Net, asn: u32) -> Self {
        Self {
            vtep,
            underlay,
            asn,
        }
    }

    /// Configures the fabric BGP instance, its peer group and the VM route filter.
    #[tracing::instrument(level = "debug", skip(self), fields(vtep = %self.vtep, asn = self.asn))]
    pub async fn ensure_bgp(&self) -> Result<(), Report> {
        vtysh(&self.bgp_commands())
            .await
            .wrap_err("failed to configure the EVPN fabric in FRR")
    }

    /// Maps the VRF to its L3 VNI and advertises the VRF's per-VM routes.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn ensure_network(&self, vrf: &str, vni: u32) -> Result<(), Report> {
        vtysh(&self.network_commands(vrf, vni))
            .await
            .wrap_err_with(|| format!("failed to configure {vrf} in FRR"))
    }

    /// Re-applies the whole configuration in one vtysh run, restoring it after an FRR restart.
    pub async fn reconcile(&self, networks: &[(&str, u32)]) -> Result<(), Report> {
        let mut commands = self.bgp_commands();
        for (vrf, vni) in networks {
            commands.extend(self.network_commands(vrf, *vni));
        }
        vtysh(&commands)
            .await
            .wrap_err("failed to reconcile the FRR configuration")
    }

    fn bgp_commands(&self) -> Vec<String> {
        let Self {
            vtep,
            underlay,
            asn,
        } = self;
        vec![
            format!("ip prefix-list {VM_ROUTES} seq 10 permit 0.0.0.0/0 ge 32"),
            format!("route-map {VM_ROUTES} permit 10"),
            format!("match ip address prefix-list {VM_ROUTES}"),
            "exit".to_owned(),
            format!("router bgp {asn}"),
            format!("bgp router-id {vtep}"),
            "no bgp default ipv4-unicast".to_owned(),
            format!("neighbor {PEER_GROUP} peer-group"),
            format!("neighbor {PEER_GROUP} remote-as internal"),
            format!("neighbor {PEER_GROUP} update-source {vtep}"),
            format!("bgp listen range {underlay} peer-group {PEER_GROUP}"),
            "address-family l2vpn evpn".to_owned(),
            format!("neighbor {PEER_GROUP} activate"),
            "advertise-all-vni".to_owned(),
            "exit-address-family".to_owned(),
            "exit".to_owned(),
        ]
    }

    fn network_commands(&self, vrf: &str, vni: u32) -> Vec<String> {
        let Self { vtep, asn, .. } = self;
        vec![
            format!("vrf {vrf}"),
            format!("vni {vni}"),
            "exit-vrf".to_owned(),
            format!("router bgp {asn} vrf {vrf}"),
            format!("bgp router-id {vtep}"),
            "address-family ipv4 unicast".to_owned(),
            format!("redistribute kernel route-map {VM_ROUTES}"),
            "exit-address-family".to_owned(),
            "address-family l2vpn evpn".to_owned(),
            "advertise ipv4 unicast".to_owned(),
            "exit-address-family".to_owned(),
            "exit".to_owned(),
        ]
    }

    /// Unmaps the VRF's VNI and withdraws its routes; a missing configuration is not an error.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn withdraw_network(&self, vrf: &str, vni: u32) -> Result<(), Report> {
        // FRR refuses to remove a VRF's BGP instance while the VRF still maps an L3 VNI.
        let unmapped = vtysh(&[
            format!("vrf {vrf}"),
            format!("no vni {vni}"),
            "exit-vrf".to_owned(),
        ])
        .await
        .wrap_err_with(|| format!("failed to unmap VNI {vni} from {vrf} in FRR"));
        let removed = match vtysh(&[format!("no router bgp {} vrf {vrf}", self.asn)]).await {
            Err(err)
                if err
                    .downcast_ref::<VtyshError>()
                    .is_some_and(|err| err.0.contains(MISSING_BGP_INSTANCE)) =>
            {
                Ok(())
            }
            result => result.wrap_err_with(|| format!("failed to remove BGP for {vrf} in FRR")),
        };
        unmapped.and(removed)
    }

    /// Drops the VRF's stanza from FRR; FRR accepts this only once the kernel VRF is gone.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn forget_vrf(&self, vrf: &str) -> Result<(), Report> {
        vtysh(&[format!("no vrf {vrf}")])
            .await
            .wrap_err_with(|| format!("failed to remove {vrf} from FRR"))
    }
}

/// Runs `commands` in vtysh's configuration mode as one invocation.
async fn vtysh(commands: &[String]) -> Result<(), Report> {
    debug!(?commands, "applying FRR configuration");
    let mut vtysh = tokio::process::Command::new("vtysh");
    vtysh.args(["-c", "configure terminal"]);
    for command in commands {
        vtysh.args(["-c", command]);
    }
    vtysh.kill_on_drop(true);
    let output = tokio::time::timeout(VTYSH_TIMEOUT, vtysh.output())
        .await
        .map_err(|_| eyre!("vtysh did not finish within {VTYSH_TIMEOUT:?}"))?
        .wrap_err("failed to run vtysh")?;
    if output.status.success() {
        return Ok(());
    }
    // FRR's own errors go to stdout; connection failures such as a stopped daemon go to stderr.
    let message = [&output.stdout, &output.stderr]
        .into_iter()
        .map(|stream| String::from_utf8_lossy(stream).trim().to_owned())
        .find(|message| !message.is_empty())
        .unwrap_or_default();
    Err(VtyshError(message).into())
}
