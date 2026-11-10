//! EVPN fabric membership and actor-owned FRR running-configuration reconciliation.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::time::Duration;

use ipnet::Ipv4Net;
use stable_eyre::Report;
use stable_eyre::eyre::{Context as EyreContext, eyre};
use tracing::{debug, warn};

use crate::messages::network::VtepInfo;

const VM_ROUTES: &str = "odorobo-vm-routes";
const PEER_GROUP: &str = "fabric";
/// This group and its explicit member configuration are reserved for odorobo.
const MESH_PEER_GROUP: &str = "odorobo-mesh";
const MISSING_BGP_INSTANCE: &str = "Can't find BGP instance";
const VTYSH_TIMEOUT: Duration = Duration::from_secs(30);

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

    pub const fn info(&self) -> VtepInfo {
        VtepInfo {
            vtep: self.vtep,
            underlay: self.underlay,
            asn: self.asn,
        }
    }

    #[tracing::instrument(level = "debug", skip(self), fields(vtep = %self.vtep, asn = self.asn))]
    pub async fn ensure_bgp(&self) -> Result<(), Report> {
        apply_vtysh(&self.bgp_commands())
            .await
            .wrap_err("failed to configure the EVPN fabric in FRR")
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn ensure_network(&self, vrf: &str, vni: u32) -> Result<(), Report> {
        let mut commands = self.bgp_commands();
        commands.extend(self.network_commands(vrf, vni));
        apply_vtysh(&commands)
            .await
            .wrap_err_with(|| format!("failed to configure {vrf} in FRR"))
    }

    pub async fn owned_neighbors(&self) -> Result<BTreeSet<Ipv4Addr>, Report> {
        let config = run_vtysh(&["show running-config".to_owned()])
            .await
            .wrap_err("failed to query managed FRR neighbors")?;
        Ok(parse_neighbors(&config, self.asn)
            .wrap_err("failed to inventory managed FRR neighbors")?
            .owned)
    }

    #[tracing::instrument(level = "debug", skip(self, networks), fields(vtep = %self.vtep, asn = self.asn))]
    pub async fn reconcile(
        &self,
        networks: &[(&str, u32)],
        peers: &BTreeSet<Ipv4Addr>,
    ) -> Result<(), Report> {
        if peers.iter().any(|peer| !self.peer_valid(*peer)) {
            return Err(eyre!("invalid desired EVPN peer for {}", self.underlay));
        }
        let config = run_vtysh(&["show running-config".to_owned()])
            .await
            .wrap_err("failed to query FRR configuration before reconciliation")?;
        let mut inventory = parse_neighbors(&config, self.asn)
            .wrap_err("failed to inventory FRR neighbors before reconciliation")?;
        let summary = run_vtysh(&["show bgp l2vpn evpn summary json".to_owned()])
            .await
            .wrap_err("failed to query EVPN sessions")?;
        let runtime = parse_runtime_peers(&summary, self.asn)?;
        inventory
            .owned
            .retain(|peer| !runtime.dynamic.contains(peer));
        inventory.foreign.extend(runtime.dynamic);
        let mut retained = peers.clone();
        retained.extend(
            inventory
                .owned
                .intersection(&runtime.established)
                .filter(|peer| self.peer_valid(**peer))
                .copied(),
        );
        apply_vtysh(&self.reconcile_commands(networks, &retained, &inventory))
            .await
            .wrap_err("failed to reconcile the FRR configuration")
    }

    fn reconcile_commands(
        &self,
        networks: &[(&str, u32)],
        peers: &BTreeSet<Ipv4Addr>,
        inventory: &NeighborInventory,
    ) -> Vec<String> {
        let initiated: BTreeSet<_> = peers.range(..self.vtep).copied().collect();
        let mut commands = self.bgp_commands();
        commands.push(format!("router bgp {}", self.asn));
        for peer in inventory.owned.difference(&initiated) {
            commands.push(format!("no neighbor {peer}"));
        }
        for peer in &initiated {
            if inventory.foreign.contains(peer) {
                warn!(%peer, "leaving operator FRR neighbor outside the automatic mesh");
                continue;
            }
            commands.push(format!("neighbor {peer} peer-group {MESH_PEER_GROUP}"));
        }
        commands.push("exit".to_owned());
        for (vrf, vni) in networks {
            commands.extend(self.network_commands(vrf, *vni));
        }
        commands
    }

    fn peer_valid(&self, peer: Ipv4Addr) -> bool {
        self.underlay.contains(&self.vtep) && self.underlay.contains(&peer) && peer != self.vtep
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
            format!("neighbor {MESH_PEER_GROUP} peer-group"),
            format!("neighbor {MESH_PEER_GROUP} remote-as internal"),
            format!("neighbor {MESH_PEER_GROUP} update-source {vtep}"),
            "address-family l2vpn evpn".to_owned(),
            format!("neighbor {PEER_GROUP} activate"),
            format!("neighbor {MESH_PEER_GROUP} activate"),
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

    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn withdraw_network(&self, vrf: &str, vni: u32) -> Result<(), Report> {
        let unmapped = apply_vtysh(&[
            format!("vrf {vrf}"),
            format!("no vni {vni}"),
            "exit-vrf".to_owned(),
        ])
        .await
        .wrap_err_with(|| format!("failed to unmap VNI {vni} from {vrf} in FRR"));
        let removed = match apply_vtysh(&[format!("no router bgp {} vrf {vrf}", self.asn)]).await {
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

    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn forget_vrf(&self, vrf: &str) -> Result<(), Report> {
        apply_vtysh(&[format!("no vrf {vrf}")])
            .await
            .wrap_err_with(|| format!("failed to remove {vrf} from FRR"))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct NeighborInventory {
    owned: BTreeSet<Ipv4Addr>,
    foreign: BTreeSet<Ipv4Addr>,
}

#[derive(Debug, Default)]
struct RuntimePeers {
    dynamic: BTreeSet<Ipv4Addr>,
    established: BTreeSet<Ipv4Addr>,
}

fn parse_runtime_peers(summary: &str, expected_asn: u32) -> Result<RuntimePeers, Report> {
    let value: serde_json::Value =
        serde_json::from_str(summary).wrap_err("invalid EVPN session response")?;
    let object = value
        .as_object()
        .ok_or_else(|| eyre!("EVPN session response is not an object"))?;
    if object.is_empty() {
        return Ok(RuntimePeers::default());
    }
    if value.get("as").and_then(serde_json::Value::as_u64) != Some(u64::from(expected_asn))
        || value.get("vrfName").and_then(serde_json::Value::as_str) != Some("default")
    {
        return Err(eyre!(
            "EVPN session response belongs to a different BGP instance"
        ));
    }
    let peers = value
        .get("peers")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| eyre!("EVPN session response has no peer inventory"))?;
    let mut result = RuntimePeers::default();
    for (address, peer) in peers {
        let Ok(address) = address.parse::<Ipv4Addr>() else {
            continue;
        };
        if peer.get("dynamicPeer").and_then(serde_json::Value::as_bool) == Some(true) {
            result.dynamic.insert(address);
        }
        if peer.get("state").and_then(serde_json::Value::as_str) == Some("Established") {
            result.established.insert(address);
        }
    }
    Ok(result)
}

fn parse_neighbors(config: &str, expected_asn: u32) -> Result<NeighborInventory, Report> {
    if config.lines().map(str::trim).rfind(|line| !line.is_empty()) != Some("end") {
        return Err(eyre!(
            "FRR running configuration is missing its final end marker"
        ));
    }
    let mut inventory = NeighborInventory::default();
    let mut in_default_bgp = false;
    let mut in_address_family = false;
    let mut saw_default_bgp = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            in_default_bgp = false;
            in_address_family = false;
            let words: Vec<_> = trimmed.split_whitespace().collect();
            if words.starts_with(&["router", "bgp"]) {
                match words.as_slice() {
                    ["router", "bgp", asn] => {
                        let asn = parse_asn(asn)?;
                        if asn != expected_asn {
                            return Err(eyre!(
                                "FRR default BGP ASN {asn} differs from {expected_asn}"
                            ));
                        }
                        if saw_default_bgp {
                            return Err(eyre!("duplicate FRR default router bgp stanza"));
                        }
                        saw_default_bgp = true;
                        in_default_bgp = true;
                    }
                    ["router", "bgp", _, "vrf" | "view", _] => {}
                    _ => return Err(eyre!("unrecognized FRR router bgp stanza: {trimmed}")),
                }
            }
            continue;
        }
        if !in_default_bgp {
            continue;
        }
        if trimmed.starts_with("address-family ") {
            in_address_family = true;
            continue;
        }
        if trimmed == "exit-address-family" {
            in_address_family = false;
            continue;
        }
        let words: Vec<_> = trimmed.split_whitespace().collect();
        let neighbor = match words.as_slice() {
            ["neighbor", target, rest @ ..] | ["no", "neighbor", target, rest @ ..] => {
                Some((*target, rest))
            }
            _ => None,
        };
        let Some((target, attributes)) = neighbor else {
            continue;
        };
        let Ok(address) = target.parse::<Ipv4Addr>() else {
            continue;
        };
        if words.first() == Some(&"neighbor")
            && !in_address_family
            && attributes == ["peer-group", MESH_PEER_GROUP]
        {
            inventory.owned.insert(address);
        } else {
            inventory.foreign.insert(address);
        }
    }
    inventory.owned = inventory
        .owned
        .difference(&inventory.foreign)
        .copied()
        .collect();
    Ok(inventory)
}

fn parse_asn(asn: &str) -> Result<u32, Report> {
    if let Some((high, low)) = asn.split_once('.') {
        let high = high
            .parse::<u16>()
            .wrap_err("invalid high part in FRR ASN")?;
        let low = low.parse::<u16>().wrap_err("invalid low part in FRR ASN")?;
        let [high_first, high_last] = high.to_be_bytes();
        let [low_first, low_last] = low.to_be_bytes();
        Ok(u32::from_be_bytes([
            high_first, high_last, low_first, low_last,
        ]))
    } else {
        asn.parse::<u32>()
            .wrap_err("invalid ASN in FRR router bgp stanza")
    }
}

async fn apply_vtysh(commands: &[String]) -> Result<(), Report> {
    debug!(?commands, "applying FRR configuration");
    let commands: Vec<_> = std::iter::once("configure terminal".to_owned())
        .chain(commands.iter().cloned())
        .collect();
    run_vtysh(&commands).await.map(|_| ())
}

async fn run_vtysh(commands: &[String]) -> Result<String, Report> {
    let mut vtysh = tokio::process::Command::new("vtysh");
    for command in commands {
        vtysh.args(["-c", command]);
    }
    vtysh.kill_on_drop(true);
    let output = tokio::time::timeout(VTYSH_TIMEOUT, vtysh.output())
        .await
        .map_err(|_| eyre!("vtysh did not finish within {VTYSH_TIMEOUT:?}"))?
        .wrap_err("failed to run vtysh")?;
    if output.status.success() {
        return String::from_utf8(output.stdout).wrap_err("vtysh output was not UTF-8");
    }
    let message = [&output.stdout, &output.stderr]
        .into_iter()
        .map(|stream| String::from_utf8_lossy(stream).trim().to_owned())
        .find(|message| !message.is_empty())
        .unwrap_or_default();
    Err(VtyshError(message).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fabric() -> Fabric {
        Fabric::new(
            "10.77.0.10".parse().unwrap(),
            "10.77.0.0/24".parse().unwrap(),
            65000,
        )
    }

    fn addresses(values: &[&str]) -> BTreeSet<Ipv4Addr> {
        values.iter().map(|value| value.parse().unwrap()).collect()
    }

    #[test]
    fn runtime_inventory_preserves_dynamic_and_established_sessions() {
        let result = parse_runtime_peers(r#"{"as":65000,"vrfName":"default","peers":{"10.77.0.2":{"dynamicPeer":true,"state":"Established"},"10.77.0.3":{"state":"Idle"}}}"#, 65000).unwrap();
        assert_eq!(result.dynamic, addresses(&["10.77.0.2"]));
        assert_eq!(result.established, addresses(&["10.77.0.2"]));
        assert_eq!(
            parse_runtime_peers("{}", 65000).unwrap().dynamic,
            BTreeSet::<Ipv4Addr>::new()
        );
        drop(
            parse_runtime_peers(r#"{"as":65001,"vrfName":"default","peers":{}}"#, 65000)
                .unwrap_err(),
        );
        drop(
            parse_runtime_peers(r#"{"as":65000,"vrfName":"tenant","peers":{}}"#, 65000)
                .unwrap_err(),
        );
    }

    #[test]
    fn inventories_only_reserved_default_instance_members() {
        let config = "Building configuration...\n\nCurrent configuration:\n!\nhostname node1\nrouter bgp 65000\n neighbor odorobo-mesh peer-group\n neighbor 10.77.0.2 peer-group odorobo-mesh\n neighbor 10.77.0.3 peer-group fabric\n neighbor 10.77.0.4 remote-as 65000\n neighbor 2001:db8::2 peer-group odorobo-mesh\n !\n address-family l2vpn evpn\n  neighbor odorobo-mesh activate\n exit-address-family\nexit\n!\nrouter bgp 65000 vrf vrf42\n neighbor 10.77.0.5 peer-group odorobo-mesh\n neighbor 10.77.0.2 peer-group operator\nexit\n!\nrouter bgp 65100 view operator\n neighbor 10.77.0.6 peer-group odorobo-mesh\nexit\n!\nend\n";
        let inventory = parse_neighbors(config, 65000).unwrap();
        assert_eq!(inventory.owned, addresses(&["10.77.0.2"]));
        assert_eq!(inventory.foreign, addresses(&["10.77.0.3", "10.77.0.4"]));
    }

    #[test]
    fn asdot_default_asn_is_normalized_and_vrf_asns_are_not_inspected() {
        let config = "router bgp 0.65000\n neighbor 10.77.0.2 peer-group odorobo-mesh\nexit\nrouter bgp 1.10 vrf operator\n neighbor 10.77.0.3 peer-group odorobo-mesh\nexit\nrouter bgp 2.20 view operator\n neighbor 10.77.0.4 peer-group odorobo-mesh\nexit\nend\n";
        assert_eq!(
            parse_neighbors(config, 65000).unwrap().owned,
            addresses(&["10.77.0.2"])
        );
        drop(parse_neighbors("router bgp 1.10\nend\n", 65000).unwrap_err());
        drop(parse_neighbors("router bgp 0.65536\nend\n", 65000).unwrap_err());
    }

    #[test]
    fn foreign_settings_override_ownership_and_malformed_queries_cannot_authorize_removal() {
        for lines in [
            " neighbor 10.77.0.2 peer-group odorobo-mesh\n neighbor 10.77.0.2 peer-group fabric\n",
            " neighbor 10.77.0.2 peer-group odorobo-mesh\n neighbor 10.77.0.2 description operator\n",
            " neighbor 10.77.0.2 peer-group odorobo-mesh\n address-family ipv4 unicast\n  neighbor 10.77.0.2 activate\n exit-address-family\n",
        ] {
            let inventory =
                parse_neighbors(&format!("router bgp 65000\n{lines}exit\nend\n"), 65000).unwrap();
            assert_eq!(inventory.owned, BTreeSet::<Ipv4Addr>::new());
            assert_eq!(inventory.foreign, addresses(&["10.77.0.2"]));
        }
        for config in [
            "",
            "router bgp 65000\n",
            "router bgp bad\nend\n",
            "router bgp 65100\nend\n",
            "router bgp 65000 unexpected\nend\n",
            "router bgp 65000\nexit\nrouter bgp 65000\nend\n",
        ] {
            drop(parse_neighbors(config, 65000).unwrap_err());
        }
        assert_eq!(
            parse_neighbors("hostname restarted-frr\nend\n", 65000).unwrap(),
            NeighborInventory::default()
        );
    }

    #[test]
    fn deltas_preserve_foreign_members_and_only_initiate_towards_lower_vteps() {
        let inventory = NeighborInventory {
            owned: addresses(&["10.77.0.2", "10.77.0.3"]),
            foreign: addresses(&["10.77.0.4"]),
        };
        let peers = addresses(&["10.77.0.2", "10.77.0.4", "10.77.0.6", "10.77.0.20"]);
        let commands = fabric().reconcile_commands(&[("vrf42", 42)], &peers, &inventory);
        let removals: Vec<_> = commands
            .iter()
            .filter(|command| command.starts_with("no neighbor "))
            .collect();
        assert_eq!(removals, vec!["no neighbor 10.77.0.3"]);
        assert!(commands.contains(&"neighbor 10.77.0.2 peer-group odorobo-mesh".to_owned()));
        assert!(commands.contains(&"neighbor 10.77.0.6 peer-group odorobo-mesh".to_owned()));
        assert!(!commands.contains(&"neighbor 10.77.0.4 peer-group odorobo-mesh".to_owned()));
        assert!(!commands.contains(&"neighbor 10.77.0.20 peer-group odorobo-mesh".to_owned()));
        assert!(commands.contains(&"router bgp 65000 vrf vrf42".to_owned()));
    }

    #[test]
    fn restarted_frr_and_partial_apply_both_recreate_desired_static_members() {
        let peers = addresses(&["10.77.0.2", "10.77.0.3"]);
        let empty = NeighborInventory::default();
        let partial = parse_neighbors(
            "router bgp 65000\n neighbor 10.77.0.2 peer-group odorobo-mesh\nexit\nend\n",
            65000,
        )
        .unwrap();
        for inventory in [&empty, &partial] {
            let commands = fabric().reconcile_commands(&[], &peers, inventory);
            for peer in &peers {
                assert!(commands.contains(&format!("neighbor {peer} peer-group odorobo-mesh")));
            }
            assert!(
                !commands
                    .iter()
                    .any(|command| command.starts_with("no neighbor "))
            );
        }
    }
}
