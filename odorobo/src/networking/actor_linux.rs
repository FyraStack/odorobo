use std::{
    collections::HashMap,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::config::{DhcpConfig, NetworkConfig, NetworkMode};
use crate::messages::network::{
    EnsureNetwork, EnsureNetworkReply, RemoveNetwork, RemoveNetworkReply,
};
use crate::network::{DhcpPool, Network, NetworkProvider};
use crate::networking::messages::{
    AttachNic, AttachedNic, DetachNic, LookupNetwork, LookupNetworkReply, NetworkStatus,
    NetworkSummary, PublishPorts, Status, UnpublishPorts,
};
use crate::ports::{PortMapping, lease_ip};
use futures_util::{StreamExt, TryStreamExt};

use kameo::{message::Context, prelude::*};
use nftables::{
    batch::Batch as NftBatch,
    expr::{Expression, Meta, MetaKey, NamedExpression, Payload, PayloadField},
    helper::{apply_ruleset, get_current_ruleset},
    schema::{Chain as NftChain, NfListObject, NfObject, Rule as NftRule, Table as NftTable},
    stmt::{Counter, Match as NftMatch, NAT, NATFamily, Operator, Statement},
    types::{NfChainPolicy, NfChainType, NfFamily, NfHook},
};
use rtnetlink::packet_route::{link::LinkMessage, route::RouteScope};
use rtnetlink::{
    Error as NetlinkError, Handle, LinkBridge, LinkUnspec, LinkVrf, RouteMessageBuilder,
};
use stable_eyre::Report;
use stable_eyre::eyre::{Context as EyreContext, eyre};
use tokio::time::sleep;
use tracing::{error, info, warn};
use ulid::Ulid;

const NFT_TABLE: &str = "odorobo";
const NFT_DNAT_CHAIN: &str = "container_dnat";
const NFT_NIC_GUARD_CHAIN: &str = "nic_guard";
const DNAT_PRIORITY: i32 = -100;
const ANTISPOOF_PRIORITY: i32 = -150;
const LEASE_TIMEOUT: Duration = Duration::from_secs(15);
const LEASE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Identifier of the network built from the `config.json` network settings.
pub const LEGACY_NETWORK_ID: &str = "default";

pub struct DhcpActor {
    pub config: DhcpConfig,
    bridge: String,
    dnsmasq_process: Option<tokio::process::Child>,
}

#[allow(clippy::unused_async_trait_impl)]
impl Actor for DhcpActor {
    type Args = (DhcpConfig, String, PathBuf);
    type Error = Report;
    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let (config, bridge, lease_file) = args;
        let dhcp_range = format!(
            "{},{},{},{}",
            config.range.0,
            config.range.1,
            config.subnet.netmask(),
            config.lease_time
        );
        if let Some(parent) = lease_file.parent() {
            std::fs::create_dir_all(parent).wrap_err_with(|| {
                format!("failed to create lease directory {}", parent.display())
            })?;
        }
        // Stale leases from a previous agent run must not publish old addresses.
        std::fs::remove_file(&lease_file).ok();

        info!(
            bridge = %bridge,
            range_start = %config.range.0,
            range_end = %config.range.1,
            subnet = %config.subnet,
            lease_time = %config.lease_time,
            lease_file = %lease_file.display(),
            "starting dnsmasq for guest DHCP"
        );

        let dnsmasq_process = tokio::process::Command::new("dnsmasq")
            .arg("--interface")
            .arg(&bridge)
            .arg("--bind-interfaces")
            .arg("--dhcp-range")
            .arg(&dhcp_range)
            .arg(format!("--dhcp-leasefile={}", lease_file.display()))
            .arg("--no-daemon")
            .spawn()
            .wrap_err_with(|| format!("failed to start dnsmasq on bridge {bridge}"))?;

        Ok(Self {
            config,
            bridge,
            dnsmasq_process: Some(dnsmasq_process),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> std::result::Result<(), Self::Error> {
        if let Some(mut dnsmasq_process) = self.dnsmasq_process.take() {
            dnsmasq_process
                .start_kill()
                .wrap_err_with(|| format!("failed to stop dnsmasq on bridge {}", self.bridge))?;

            _ = dnsmasq_process.wait().await;
        }

        Ok(())
    }
}

/// A NIC attached to a realized network.
struct Member {
    vmid: Ulid,
    tap_name: String,
    mac_address: Option<String>,
    address: Option<Ipv4Addr>,
}

struct RoutedState {
    vrf: String,
    vrf_index: u32,
    table_id: u32,
    gateway: Ipv4Addr,
    mtu: u16,
    members: Vec<Member>,
}

struct BridgedState {
    bridge: String,
    dhcp_actor: Option<ActorRef<DhcpActor>>,
    lease_file: PathBuf,
    members: Vec<Member>,
}

enum Realized {
    Routed(RoutedState),
    Bridged(BridgedState),
}

struct NetworkEntry {
    network: Network,
    realized: Realized,
}

impl NetworkEntry {
    fn members(&self) -> &[Member] {
        match &self.realized {
            Realized::Routed(state) => &state.members,
            Realized::Bridged(state) => &state.members,
        }
    }

    const fn members_mut(&mut self) -> &mut Vec<Member> {
        match &mut self.realized {
            Realized::Routed(state) => &mut state.members,
            Realized::Bridged(state) => &mut state.members,
        }
    }
}

/// Builds the legacy network object from the `config.json` network settings.
fn legacy_network(config: &NetworkConfig) -> Network {
    let (bridge, subnet, gateway, upstream) = match &config.network_mode {
        NetworkMode::HostonlyNat {
            bridge,
            subnet,
            gateway,
            upstream_iface,
        } => (
            bridge.clone(),
            *subnet,
            *gateway,
            Some(upstream_iface.clone()),
        ),
        NetworkMode::Bridged {
            bridge,
            subnet,
            gateway,
        } => (bridge.clone(), *subnet, *gateway, None),
    };

    Network {
        id: LEGACY_NETWORK_ID.to_owned(),
        subnet,
        gateway: Some(gateway),
        provider: NetworkProvider::Bridged,
        vni: None,
        mtu: None,
        bridge: Some(bridge),
        dhcp: config.dhcp_config.as_ref().map(|dhcp| DhcpPool {
            start: dhcp.range.0,
            end: dhcp.range.1,
            lease_time: dhcp.lease_time.clone(),
        }),
        upstream,
    }
}

fn write_sysctl(key: &str, value: &str) -> Result<(), Report> {
    let path = format!("/proc/sys/{key}");
    std::fs::write(&path, value).wrap_err_with(|| format!("failed to write {value} to {path}"))
}

/// Host-wide settings the routed model depends on.
fn enable_host_forwarding() -> Result<(), Report> {
    write_sysctl("net/ipv4/ip_forward", "1")?;
    write_sysctl("net/ipv4/conf/all/rp_filter", "0")?;
    Ok(())
}

/// Per-TAP settings that make the anycast gateway work on every network.
///
/// Proxy ARP answers the peer ARPs the guest sends because its kernel cannot
/// express `/32` on `ip=` (`255.255.255.255` is `INADDR_NONE`: the kernel treats
/// it as unset and guesses a classful mask).
fn write_tap_sysctls(tap_name: &str) -> Result<(), Report> {
    for (suffix, value) in [
        ("forwarding", "1"),
        ("rp_filter", "0"),
        ("arp_ignore", "1"),
        ("arp_announce", "2"),
        ("proxy_arp", "1"),
    ] {
        write_sysctl(&format!("net/ipv4/conf/{tap_name}/{suffix}"), value)?;
    }
    Ok(())
}

async fn lookup_link(handle: &Handle, link_name: &str) -> Result<Option<LinkMessage>, Report> {
    match handle
        .link()
        .get()
        .match_name(link_name.to_owned())
        .execute()
        .next()
        .await
    {
        Some(Ok(link)) => Ok(Some(link)),
        Some(Err(NetlinkError::NetlinkError(err))) if err.raw_code() == -libc::ENODEV => Ok(None),
        Some(Err(err)) => Err(err).wrap_err_with(|| format!("failed to query link {link_name}")),
        None => Ok(None),
    }
}

async fn ensure_bridge(handle: &Handle, name: &str) -> Result<u32, Report> {
    if let Some(link) = lookup_link(handle, name).await? {
        return Ok(link.header.index);
    }

    info!(bridge = name, "bridge not found, creating it");
    handle
        .link()
        .add(LinkBridge::new(name).up().build())
        .execute()
        .await
        .wrap_err_with(|| format!("failed to create bridge {name}"))?;

    let link = lookup_link(handle, name)
        .await?
        .ok_or_else(|| eyre!("bridge {name} was not found after creation"))?;
    Ok(link.header.index)
}

async fn ensure_address(
    handle: &Handle,
    link_index: u32,
    link_name: &str,
    address: Ipv4Addr,
    prefix_len: u8,
) -> Result<(), Report> {
    let mut existing = handle
        .address()
        .get()
        .set_link_index_filter(link_index)
        .execute();

    while let Some(address_msg) = existing.try_next().await? {
        if address_msg.header.prefix_len != prefix_len {
            continue;
        }

        let already_present = address_msg.attributes.iter().any(|attr| match attr {
            rtnetlink::packet_route::address::AddressAttribute::Address(ip)
            | rtnetlink::packet_route::address::AddressAttribute::Local(ip) => {
                matches!(ip, std::net::IpAddr::V4(existing_ip) if *existing_ip == address)
            }
            _ => false,
        });

        if already_present {
            info!(
                link = link_name,
                address = %address,
                prefix_len,
                "address already present on link"
            );
            return Ok(());
        }
    }

    info!(
        link = link_name,
        address = %address,
        prefix_len,
        "adding address to link"
    );

    match handle
        .address()
        .add(link_index, address.into(), prefix_len)
        .execute()
        .await
    {
        Ok(()) => Ok(()),
        Err(NetlinkError::NetlinkError(err)) if err.raw_code() == -libc::EEXIST => Ok(()),
        Err(err) => Err(err).wrap_err_with(|| {
            format!("failed to add address {address}/{prefix_len} to {link_name}")
        }),
    }
}

/// Target address used when publishing ports for a VM.
enum PublishTarget {
    Static(Ipv4Addr),
    Lease(PathBuf),
    None,
}

#[derive(RemoteActor)]
pub struct NetworkAgentActor {
    pub config: NetworkConfig,
    runtime_root: PathBuf,
    networks: HashMap<String, NetworkEntry>,
    port_tasks: HashMap<Ulid, tokio::task::JoinHandle<()>>,
    netlink_thread: tokio::task::JoinHandle<()>,
    netlink_handle: Handle,
}

impl Actor for NetworkAgentActor {
    type Args = (NetworkConfig, PathBuf);
    type Error = Report;
    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let (config, runtime_root) = args;
        let (connection, handle, _) = rtnetlink::new_connection()?;
        let netlink_thread = tokio::spawn(connection);

        let legacy = legacy_network(&config);
        info!(
            network_id = %legacy.id,
            provider = ?legacy.provider,
            subnet = %legacy.subnet,
            bridge = ?legacy.bridge,
            "realizing the legacy network from config.json"
        );

        let mut actor = Self {
            config,
            runtime_root,
            networks: HashMap::new(),
            port_tasks: HashMap::new(),
            netlink_thread,
            netlink_handle: handle,
        };

        actor
            .ensure_network(&actor_ref, legacy)
            .await
            .wrap_err("failed to realize the legacy network from config.json")?;

        enable_host_forwarding().wrap_err("failed to enable host forwarding settings")?;
        Self::clear_port_rules().wrap_err("failed to clear stale port rules")?;
        Self::prune_stale_antispoof_rules(&actor.netlink_handle)
            .await
            .wrap_err("failed to prune stale anti-spoof rules")?;

        Ok(actor)
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> std::result::Result<(), Self::Error> {
        match reason {
            ActorStopReason::Normal => {
                info!("stopping network agent");
            }
            ActorStopReason::Killed => {
                info!("network agent killed");
            }
            ActorStopReason::Panicked(err) => {
                info!(?err, "network agent panicked");
            }
            _ => {
                info!("network agent stopping");
            }
        }

        for (id, entry) in &mut self.networks {
            if let Realized::Bridged(state) = &mut entry.realized
                && let Some(dhcp_actor) = state.dhcp_actor.take()
            {
                info!(network_id = %id, bridge = %state.bridge, "stopping dnsmasq");
                dhcp_actor.stop_gracefully().await?;
            }
        }

        for (_, task) in self.port_tasks.drain() {
            task.abort();
            _ = task.await;
        }

        self.netlink_thread.abort();
        _ = (&mut self.netlink_thread).await;

        Ok(())
    }
}

impl NetworkAgentActor {
    fn lease_file_for(&self, network_id: &str) -> PathBuf {
        self.runtime_root
            .join(format!("dnsmasq-{network_id}.leases"))
    }

    async fn ensure_network(
        &mut self,
        actor_ref: &ActorRef<Self>,
        network: Network,
    ) -> Result<(), Report> {
        network.validate().map_err(|err| eyre!("{err}"))?;

        if let Some(existing) = self.networks.get(&network.id) {
            if existing.network != network {
                return Err(eyre!(
                    "network {:?} already exists with different desired state",
                    network.id
                ));
            }
            return Ok(());
        }

        let realized = match network.provider {
            NetworkProvider::Routed => Realized::Routed(self.realize_routed(&network).await?),
            NetworkProvider::Bridged => {
                Realized::Bridged(self.realize_bridged(actor_ref, &network).await?)
            }
        };

        info!(
            network_id = %network.id,
            provider = ?network.provider,
            subnet = %network.subnet,
            "realized network"
        );
        self.networks
            .insert(network.id.clone(), NetworkEntry { network, realized });
        Ok(())
    }

    async fn realize_routed(&self, network: &Network) -> Result<RoutedState, Report> {
        let vni = network
            .vni
            .ok_or_else(|| eyre!("routed network {:?} requires a vni", network.id))?;
        let vrf = format!("vrf{vni}");

        let vrf_index = if let Some(link) = lookup_link(&self.netlink_handle, &vrf).await? {
            link.header.index
        } else {
            info!(vrf = %vrf, table = vni, "creating vrf device for routed network");
            self.netlink_handle
                .link()
                .add(LinkVrf::new(&vrf, vni).up().build())
                .execute()
                .await
                .wrap_err_with(|| format!("failed to create vrf device {vrf}"))?;

            lookup_link(&self.netlink_handle, &vrf)
                .await?
                .ok_or_else(|| eyre!("vrf device {vrf} was not found after creation"))?
                .header
                .index
        };

        let state = RoutedState {
            vrf,
            vrf_index,
            table_id: vni,
            gateway: network.gateway(),
            mtu: network.mtu(),
            members: Vec::new(),
        };

        info!(
            network_id = %network.id,
            vrf = %state.vrf,
            table = vni,
            gateway = %state.gateway,
            "routed network realized"
        );
        Ok(state)
    }

    async fn realize_bridged(
        &self,
        actor_ref: &ActorRef<Self>,
        network: &Network,
    ) -> Result<BridgedState, Report> {
        let bridge = network
            .bridge
            .clone()
            .ok_or_else(|| eyre!("bridged network {:?} requires a bridge", network.id))?;
        let bridge_index = ensure_bridge(&self.netlink_handle, &bridge).await?;
        ensure_address(
            &self.netlink_handle,
            bridge_index,
            &bridge,
            network.gateway(),
            network.subnet.prefix_len(),
        )
        .await?;

        let lease_file = self.lease_file_for(&network.id);
        if let Some(upstream) = &network.upstream {
            Self::ensure_nat_rules(&bridge, &network.subnet.to_string(), upstream)?;
        }
        let dhcp_actor = if let Some(dhcp) = &network.dhcp {
            let config = DhcpConfig {
                range: (dhcp.start, dhcp.end),
                subnet: network.subnet,
                lease_time: dhcp.lease_time.clone(),
            };
            Some(
                DhcpActor::spawn_link(actor_ref, (config, bridge.clone(), lease_file.clone()))
                    .await,
            )
        } else {
            None
        };

        Ok(BridgedState {
            bridge,
            dhcp_actor,
            lease_file,
            members: Vec::new(),
        })
    }

    async fn remove_network(&mut self, id: &str) -> Result<(), Report> {
        let Some(entry) = self.networks.get(id) else {
            return Err(eyre!("unknown network {id}"));
        };
        if !entry.members().is_empty() {
            return Err(eyre!("network {id} still has attached NICs"));
        }

        let Some(entry) = self.networks.remove(id) else {
            return Err(eyre!("unknown network {id}"));
        };

        match entry.realized {
            Realized::Bridged(mut state) => {
                if let Some(dhcp_actor) = state.dhcp_actor.take() {
                    dhcp_actor.stop_gracefully().await?;
                }
                info!(network_id = %id, bridge = %state.bridge, "removed bridged network");
            }
            Realized::Routed(state) => {
                self.netlink_handle
                    .link()
                    .del(state.vrf_index)
                    .execute()
                    .await
                    .wrap_err_with(|| format!("failed to delete vrf device {}", state.vrf))?;
                info!(network_id = %id, vrf = %state.vrf, "removed routed network");
            }
        }

        Ok(())
    }

    async fn attach_nic(&mut self, msg: AttachNic) -> Result<(), Report> {
        let alias = format!("odorobo {} {}", msg.vmid, msg.network_id);
        let Some(entry) = self.networks.get_mut(&msg.network_id) else {
            return Err(eyre!("unknown network {}", msg.network_id));
        };

        if entry
            .members()
            .iter()
            .any(|member| member.vmid == msg.vmid && member.tap_name == msg.tap_name)
        {
            info!(
                vmid = %msg.vmid,
                tap = %msg.tap_name,
                network_id = %msg.network_id,
                "NIC already attached"
            );
            return Ok(());
        }

        match &mut entry.realized {
            Realized::Routed(state) => {
                Self::attach_routed(&self.netlink_handle, state, &msg, &alias).await?;
            }
            Realized::Bridged(state) => {
                Self::attach_bridged(&self.netlink_handle, state, &msg, &alias).await?;
            }
        }

        info!(
            vmid = %msg.vmid,
            tap = %msg.tap_name,
            network_id = %msg.network_id,
            "attached NIC"
        );
        Ok(())
    }

    async fn attach_routed(
        handle: &Handle,
        state: &mut RoutedState,
        msg: &AttachNic,
        alias: &str,
    ) -> Result<(), Report> {
        let address = msg.address.ok_or_else(|| {
            eyre!(
                "network {} is routed; NIC for VM {} requires a static address",
                msg.network_id,
                msg.vmid
            )
        })?;

        let tap = lookup_link(handle, &msg.tap_name)
            .await?
            .ok_or_else(|| eyre!("tap {} not found", msg.tap_name))?;
        let tap_index = tap.header.index;

        handle
            .link()
            .set(
                LinkUnspec::new_with_index(tap_index)
                    .alias(alias.to_owned())
                    .controller(state.vrf_index)
                    .mtu(u32::from(state.mtu))
                    .up()
                    .build(),
            )
            .execute()
            .await
            .wrap_err_with(|| {
                format!("failed to attach tap {} to vrf {}", msg.tap_name, state.vrf)
            })?;

        ensure_address(handle, tap_index, &msg.tap_name, state.gateway, 32).await?;

        handle
            .route()
            .add(
                RouteMessageBuilder::<Ipv4Addr>::new()
                    .table_id(state.table_id)
                    .output_interface(tap_index)
                    .scope(RouteScope::Link)
                    .destination_prefix(address, 32)
                    .build(),
            )
            .execute()
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to add route {address}/32 via tap {} to table {}",
                    msg.tap_name, state.table_id
                )
            })?;

        write_tap_sysctls(&msg.tap_name)?;
        Self::ensure_antispoof_rule(&msg.tap_name, address)?;

        state.members.push(Member {
            vmid: msg.vmid,
            tap_name: msg.tap_name.clone(),
            mac_address: msg.mac_address.clone(),
            address: Some(address),
        });
        Ok(())
    }

    async fn attach_bridged(
        handle: &Handle,
        state: &mut BridgedState,
        msg: &AttachNic,
        alias: &str,
    ) -> Result<(), Report> {
        let bridge = lookup_link(handle, &state.bridge)
            .await?
            .ok_or_else(|| eyre!("bridge {} not found", state.bridge))?;
        let tap = lookup_link(handle, &msg.tap_name)
            .await?
            .ok_or_else(|| eyre!("tap {} not found", msg.tap_name))?;

        handle
            .link()
            .set(
                LinkUnspec::new_with_index(tap.header.index)
                    .alias(alias.to_owned())
                    .controller(bridge.header.index)
                    .up()
                    .build(),
            )
            .execute()
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to attach tap {} to bridge {}",
                    msg.tap_name, state.bridge
                )
            })?;

        state.members.push(Member {
            vmid: msg.vmid,
            tap_name: msg.tap_name.clone(),
            mac_address: msg.mac_address.clone(),
            address: None,
        });
        Ok(())
    }

    async fn detach_nic(&mut self, msg: DetachNic) -> Result<(), Report> {
        let mut removed = None;
        for (id, entry) in &mut self.networks {
            let members = entry.members_mut();
            if let Some(index) = members
                .iter()
                .position(|member| member.vmid == msg.vmid && member.tap_name == msg.tap_name)
            {
                removed = Some((id.clone(), members.remove(index)));
                break;
            }
        }

        let Some((network_id, member)) = removed else {
            info!(
                vmid = %msg.vmid,
                tap = %msg.tap_name,
                "no attachment recorded; nothing to detach"
            );
            return Ok(());
        };

        Self::remove_antispoof_rules(&member.tap_name)?;

        let tap = lookup_link(&self.netlink_handle, &member.tap_name).await?;
        if let Some(tap) = tap {
            self.netlink_handle
                .link()
                .set(
                    LinkUnspec::new_with_index(tap.header.index)
                        .nocontroller()
                        .build(),
                )
                .execute()
                .await
                .wrap_err_with(|| format!("failed to detach tap {}", member.tap_name))?;
        }

        info!(
            vmid = %msg.vmid,
            tap = %member.tap_name,
            network_id = %network_id,
            "detached NIC"
        );
        Ok(())
    }

    fn publish_target(&self, vmid: Ulid, mac_address: &str) -> PublishTarget {
        for entry in self.networks.values() {
            match &entry.realized {
                Realized::Routed(state) => {
                    if let Some(member) = state.members.iter().find(|member| {
                        member.vmid == vmid && member.mac_address.as_deref() == Some(mac_address)
                    }) && let Some(address) = member.address
                    {
                        return PublishTarget::Static(address);
                    }
                }
                Realized::Bridged(state) => {
                    if state.members.iter().any(|member| {
                        member.vmid == vmid && member.mac_address.as_deref() == Some(mac_address)
                    }) {
                        return PublishTarget::Lease(state.lease_file.clone());
                    }
                }
            }
        }
        PublishTarget::None
    }

    fn nft_table_exists(objects: &[NfObject<'_>], table: &str) -> bool {
        objects.iter().any(|object| match object {
            NfObject::ListObject(NfListObject::Table(existing)) => {
                existing.family == NfFamily::IP && existing.name.as_ref() == table
            }
            _ => false,
        })
    }

    fn nft_chain_exists(objects: &[NfObject<'_>], table: &str, chain: &str) -> bool {
        objects.iter().any(|object| match object {
            NfObject::ListObject(NfListObject::Chain(existing)) => {
                existing.family == NfFamily::IP
                    && existing.table.as_ref() == table
                    && existing.name.as_ref() == chain
            }
            _ => false,
        })
    }

    fn nft_postrouting_masquerade_exists(
        objects: &[NfObject<'_>],
        table: &str,
        chain: &str,
        upstream_iface: &str,
    ) -> bool {
        objects.iter().any(|object| match object {
            NfObject::ListObject(NfListObject::Rule(existing)) => {
                existing.family == NfFamily::IP
                    && existing.table.as_ref() == table
                    && existing.chain.as_ref() == chain
                    && matches!(
                        existing.expr.as_ref(),
                        [Statement::Match(NftMatch { left, right, op }), Statement::Masquerade(_)]
                            if *op == Operator::EQ
                                && matches!(
                                    left,
                                    Expression::Named(NamedExpression::Meta(Meta { key: MetaKey::Oifname }))
                                )
                                && matches!(right, Expression::String(iface) if iface.as_ref() == upstream_iface)
                    )
            }
            _ => false,
        })
    }

    /// Ensures the host-only NAT masquerade rule exists for the configured
    /// upstream interface.
    ///
    /// This is intentionally IPv4-only for now: guest subnet config uses
    /// `Ipv4Net`, bridge gateways use `Ipv4Addr`, and switching the masquerade
    /// rule to dual-stack would implicitly opt into IPv6 NAT policy.
    fn ensure_nat_rules(_bridge: &str, _subnet: &str, upstream_iface: &str) -> Result<(), Report> {
        const TABLE_NAME: &str = "odorobo";
        const CHAIN_NAME: &str = "guest_nat";

        info!(
            table = TABLE_NAME,
            chain = CHAIN_NAME,
            upstream_iface = upstream_iface,
            "ensuring host-only NAT masquerade rule exists"
        );

        let ruleset = get_current_ruleset().wrap_err("failed to fetch current nftables ruleset")?;
        let objects = ruleset.objects.as_ref();

        let table_exists = Self::nft_table_exists(objects, TABLE_NAME);
        let chain_exists = Self::nft_chain_exists(objects, TABLE_NAME, CHAIN_NAME);
        let rule_exists = Self::nft_postrouting_masquerade_exists(
            objects,
            TABLE_NAME,
            CHAIN_NAME,
            upstream_iface,
        );

        if rule_exists {
            info!(
                table = TABLE_NAME,
                chain = CHAIN_NAME,
                upstream_iface = upstream_iface,
                "existing NAT masquerade rule already present"
            );
            return Ok(());
        }

        let mut batch = NftBatch::new();

        if !table_exists {
            info!(table = TABLE_NAME, "creating odorobo nftables table");
            batch.add(NfListObject::Table(NftTable {
                family: NfFamily::IP,
                name: TABLE_NAME.into(),
                handle: None,
            }));
        }

        if !chain_exists {
            info!(
                table = TABLE_NAME,
                chain = CHAIN_NAME,
                "creating odorobo nftables chain"
            );
            batch.add(NfListObject::Chain(NftChain {
                family: NfFamily::IP,
                table: TABLE_NAME.into(),
                name: CHAIN_NAME.into(),
                newname: None,
                handle: None,
                _type: Some(NfChainType::NAT),
                hook: Some(NfHook::Postrouting),
                prio: Some(100),
                dev: None,
                policy: Some(NfChainPolicy::Accept),
            }));
        }

        batch.add(NfListObject::Rule(NftRule {
            family: NfFamily::IP,
            table: TABLE_NAME.into(),
            chain: CHAIN_NAME.into(),
            expr: vec![
                Statement::Match(NftMatch {
                    left: Expression::Named(NamedExpression::Meta(Meta {
                        key: MetaKey::Oifname,
                    })),
                    right: Expression::String(upstream_iface.into()),
                    op: Operator::EQ,
                }),
                Statement::Masquerade(None),
            ]
            .into(),
            handle: None,
            index: None,
            comment: Some("odorobo host-only NAT".into()),
        }));

        let batch = batch.to_nftables();
        apply_ruleset(&batch).wrap_err_with(|| {
            format!(
                "failed to apply nftables postrouting masquerade for upstream interface {upstream_iface}"
            )
        })?;

        Ok(())
    }

    fn ensure_antispoof_chain() -> Result<(), Report> {
        let ruleset =
            get_current_ruleset().wrap_err("failed to fetch nftables ruleset for NIC guard")?;
        let objects = ruleset.objects.as_ref();
        if Self::nft_chain_exists(objects, NFT_TABLE, NFT_NIC_GUARD_CHAIN) {
            return Ok(());
        }

        let mut batch = NftBatch::new();
        if !Self::nft_table_exists(objects, NFT_TABLE) {
            batch.add(NfListObject::Table(NftTable {
                family: NfFamily::IP,
                name: NFT_TABLE.into(),
                handle: None,
            }));
        }
        batch.add(NfListObject::Chain(NftChain {
            family: NfFamily::IP,
            table: NFT_TABLE.into(),
            name: NFT_NIC_GUARD_CHAIN.into(),
            newname: None,
            handle: None,
            _type: Some(NfChainType::Filter),
            hook: Some(NfHook::Prerouting),
            prio: Some(ANTISPOOF_PRIORITY),
            dev: None,
            policy: Some(NfChainPolicy::Accept),
        }));
        apply_ruleset(&batch.to_nftables()).wrap_err("failed to create the NIC guard chain")?;
        info!(
            table = NFT_TABLE,
            chain = NFT_NIC_GUARD_CHAIN,
            "created NIC guard chain"
        );
        Ok(())
    }

    /// Deletes anti-spoof rules whose TAP no longer exists (rules outlive process restarts).
    async fn prune_stale_antispoof_rules(handle: &Handle) -> Result<(), Report> {
        let ruleset = get_current_ruleset()
            .wrap_err("failed to fetch nftables ruleset for stale anti-spoof rules")?;
        let mut batch = NftBatch::new();
        for object in ruleset.objects.as_ref() {
            let NfObject::ListObject(NfListObject::Rule(rule)) = object else {
                continue;
            };
            if rule.family != NfFamily::IP
                || rule.table.as_ref() != NFT_TABLE
                || rule.chain.as_ref() != NFT_NIC_GUARD_CHAIN
            {
                continue;
            }
            let Some(tap_name) = rule
                .comment
                .as_deref()
                .and_then(|comment| comment.strip_prefix("odorobo antispoof "))
                .and_then(|rest| rest.split_whitespace().next())
            else {
                continue;
            };
            if lookup_link(handle, tap_name).await?.is_some() {
                continue;
            }
            let Some(rule_handle) = rule.handle else {
                continue;
            };
            batch.delete(NfListObject::Rule(NftRule {
                family: NfFamily::IP,
                table: NFT_TABLE.into(),
                chain: rule.chain.clone(),
                expr: Vec::new().into(),
                handle: Some(rule_handle),
                index: None,
                comment: None,
            }));
            info!(tap = tap_name, "pruned stale anti-spoof rule");
        }
        apply_ruleset(&batch.to_nftables()).wrap_err("failed to prune stale anti-spoof rules")
    }

    fn antispoof_rule(tap_name: &str, address: Ipv4Addr) -> NftRule<'static> {
        NftRule {
            family: NfFamily::IP,
            table: NFT_TABLE.into(),
            chain: NFT_NIC_GUARD_CHAIN.into(),
            expr: vec![
                Statement::Match(NftMatch {
                    left: Expression::Named(NamedExpression::Meta(Meta {
                        key: MetaKey::Iifname,
                    })),
                    right: Expression::String(tap_name.to_owned().into()),
                    op: Operator::EQ,
                }),
                Statement::Match(NftMatch {
                    left: Expression::Named(NamedExpression::Payload(Payload::PayloadField(
                        PayloadField {
                            protocol: "ip".into(),
                            field: "saddr".into(),
                        },
                    ))),
                    right: Expression::String(address.to_string().into()),
                    op: Operator::NEQ,
                }),
                Statement::Counter(Counter::Anonymous(None)),
                Statement::Drop(None),
            ]
            .into(),
            handle: None,
            index: None,
            comment: Some(format!("odorobo antispoof {tap_name} {address}").into()),
        }
    }

    /// Replaces the anti-spoof rules of a TAP with a rule for `address`.
    fn ensure_antispoof_rule(tap_name: &str, address: Ipv4Addr) -> Result<(), Report> {
        Self::ensure_antispoof_chain()?;

        let ruleset =
            get_current_ruleset().wrap_err("failed to fetch nftables ruleset for NIC guard")?;
        let prefix = format!("odorobo antispoof {tap_name} ");
        let expected = format!("odorobo antispoof {tap_name} {address}");
        let mut batch = NftBatch::new();
        let mut present = false;
        for object in ruleset.objects.as_ref() {
            let NfObject::ListObject(NfListObject::Rule(rule)) = object else {
                continue;
            };
            if rule.family != NfFamily::IP
                || rule.table.as_ref() != NFT_TABLE
                || rule.chain.as_ref() != NFT_NIC_GUARD_CHAIN
            {
                continue;
            }
            let Some(comment) = rule.comment.as_deref() else {
                continue;
            };
            if comment == expected {
                present = true;
                continue;
            }
            if !comment.starts_with(&prefix) {
                continue;
            }
            let Some(handle) = rule.handle else {
                continue;
            };
            batch.delete(NfListObject::Rule(NftRule {
                family: NfFamily::IP,
                table: NFT_TABLE.into(),
                chain: rule.chain.clone(),
                expr: Vec::new().into(),
                handle: Some(handle),
                index: None,
                comment: None,
            }));
        }

        if !present {
            batch.add(NfListObject::Rule(Self::antispoof_rule(tap_name, address)));
        }
        apply_ruleset(&batch.to_nftables())
            .wrap_err_with(|| format!("failed to ensure anti-spoof rules for tap {tap_name}"))?;
        Ok(())
    }

    fn remove_antispoof_rules(tap_name: &str) -> Result<(), Report> {
        let ruleset =
            get_current_ruleset().wrap_err("failed to fetch nftables ruleset for NIC guard")?;
        let prefix = format!("odorobo antispoof {tap_name} ");
        let mut batch = NftBatch::new();
        let mut removed = 0_u32;
        for object in ruleset.objects.as_ref() {
            let NfObject::ListObject(NfListObject::Rule(rule)) = object else {
                continue;
            };
            if rule.family != NfFamily::IP
                || rule.table.as_ref() != NFT_TABLE
                || rule.chain.as_ref() != NFT_NIC_GUARD_CHAIN
                || !rule
                    .comment
                    .as_deref()
                    .is_some_and(|comment| comment.starts_with(&prefix))
            {
                continue;
            }
            let Some(handle) = rule.handle else {
                continue;
            };
            batch.delete(NfListObject::Rule(NftRule {
                family: NfFamily::IP,
                table: NFT_TABLE.into(),
                chain: rule.chain.clone(),
                expr: Vec::new().into(),
                handle: Some(handle),
                index: None,
                comment: None,
            }));
            removed = removed.saturating_add(1);
        }
        if removed > 0 {
            apply_ruleset(&batch.to_nftables())
                .wrap_err_with(|| format!("failed to remove anti-spoof rules for {tap_name}"))?;
            info!(tap = tap_name, rules = removed, "removed anti-spoof rules");
        }
        Ok(())
    }

    fn ensure_port_forward_chain() -> Result<(), Report> {
        let ruleset = get_current_ruleset()
            .wrap_err("failed to fetch nftables ruleset for port publishing")?;
        let objects = ruleset.objects.as_ref();
        if Self::nft_chain_exists(objects, NFT_TABLE, NFT_DNAT_CHAIN) {
            return Ok(());
        }

        let mut batch = NftBatch::new();
        if !Self::nft_table_exists(objects, NFT_TABLE) {
            batch.add(NfListObject::Table(NftTable {
                family: NfFamily::IP,
                name: NFT_TABLE.into(),
                handle: None,
            }));
        }
        batch.add(NfListObject::Chain(NftChain {
            family: NfFamily::IP,
            table: NFT_TABLE.into(),
            name: NFT_DNAT_CHAIN.into(),
            newname: None,
            handle: None,
            _type: Some(NfChainType::NAT),
            hook: Some(NfHook::Prerouting),
            prio: Some(DNAT_PRIORITY),
            dev: None,
            policy: Some(NfChainPolicy::Accept),
        }));
        apply_ruleset(&batch.to_nftables())
            .wrap_err("failed to create the container DNAT chain")?;
        info!(
            table = NFT_TABLE,
            chain = NFT_DNAT_CHAIN,
            "created container DNAT chain"
        );
        Ok(())
    }

    fn dnat_rule(vmid: Ulid, mapping: PortMapping, address: Ipv4Addr) -> NftRule<'static> {
        NftRule {
            family: NfFamily::IP,
            table: NFT_TABLE.into(),
            chain: NFT_DNAT_CHAIN.into(),
            expr: vec![
                Statement::Match(NftMatch {
                    left: Expression::Named(NamedExpression::Payload(Payload::PayloadField(
                        PayloadField {
                            protocol: mapping.protocol.as_str().into(),
                            field: "dport".into(),
                        },
                    ))),
                    right: Expression::Number(u32::from(mapping.host)),
                    op: Operator::EQ,
                }),
                Statement::DNAT(Some(NAT {
                    addr: Some(Expression::String(address.to_string().into())),
                    family: Some(NATFamily::IP),
                    port: Some(Expression::Number(u32::from(mapping.guest))),
                    flags: None,
                })),
            ]
            .into(),
            handle: None,
            index: None,
            comment: Some(format!("odorobo container {vmid} {mapping}").into()),
        }
    }

    async fn wait_for_lease(path: &Path, mac_address: &str) -> Result<Ipv4Addr, Report> {
        let deadline = tokio::time::Instant::now()
            .checked_add(LEASE_TIMEOUT)
            .expect("deadline in range");
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(path).await
                && let Some(address) = lease_ip(&contents, mac_address)
            {
                return Ok(address);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(eyre!(
                    "no DHCP lease for {mac_address} in {} after {}s",
                    path.display(),
                    LEASE_TIMEOUT.as_secs()
                ));
            }
            sleep(LEASE_POLL_INTERVAL).await;
        }
    }

    fn publish_ports(vmid: Ulid, ports: &[PortMapping], address: Ipv4Addr) -> Result<(), Report> {
        Self::ensure_port_forward_chain()?;

        let mut batch = NftBatch::new();
        for mapping in ports {
            batch.add(NfListObject::Rule(Self::dnat_rule(vmid, *mapping, address)));
        }
        apply_ruleset(&batch.to_nftables())
            .wrap_err_with(|| format!("failed to publish ports for {vmid} to {address}"))?;
        Ok(())
    }

    fn clear_port_rules() -> Result<(), Report> {
        let ruleset = get_current_ruleset().wrap_err("failed to fetch nftables ruleset")?;
        let mut batch = NftBatch::new();
        let mut removed = 0_u32;
        for object in ruleset.objects.as_ref() {
            let NfObject::ListObject(NfListObject::Rule(rule)) = object else {
                continue;
            };
            if rule.family != NfFamily::IP
                || rule.table.as_ref() != NFT_TABLE
                || rule.chain.as_ref() != NFT_DNAT_CHAIN
            {
                continue;
            }
            let Some(handle) = rule.handle else {
                continue;
            };
            batch.delete(NfListObject::Rule(NftRule {
                family: NfFamily::IP,
                table: NFT_TABLE.into(),
                chain: rule.chain.clone(),
                expr: Vec::new().into(),
                handle: Some(handle),
                index: None,
                comment: None,
            }));
            removed = removed.saturating_add(1);
        }
        if removed > 0 {
            apply_ruleset(&batch.to_nftables()).wrap_err("failed to clear stale port rules")?;
            info!(
                rules = removed,
                "cleared stale port-forward rules from a previous run"
            );
        }
        Ok(())
    }

    fn remove_port_rules(vmid: Ulid) -> Result<(), Report> {
        let ruleset = get_current_ruleset().wrap_err("failed to fetch nftables ruleset")?;
        let prefix = format!("odorobo container {vmid} ");
        let mut batch = NftBatch::new();
        let mut removed = 0_u32;
        for object in ruleset.objects.as_ref() {
            let NfObject::ListObject(NfListObject::Rule(rule)) = object else {
                continue;
            };
            if rule.family != NfFamily::IP
                || rule.table.as_ref() != NFT_TABLE
                || !rule
                    .comment
                    .as_deref()
                    .is_some_and(|comment| comment.starts_with(&prefix))
            {
                continue;
            }
            let Some(handle) = rule.handle else {
                continue;
            };
            batch.delete(NfListObject::Rule(NftRule {
                family: NfFamily::IP,
                table: NFT_TABLE.into(),
                chain: rule.chain.clone(),
                expr: Vec::new().into(),
                handle: Some(handle),
                index: None,
                comment: None,
            }));
            removed = removed.saturating_add(1);
        }
        if removed > 0 {
            apply_ruleset(&batch.to_nftables())
                .wrap_err_with(|| format!("failed to remove port rules for {vmid}"))?;
            info!(vmid = %vmid, rules = removed, "removed port rules");
        }
        Ok(())
    }
}

impl Message<EnsureNetwork> for NetworkAgentActor {
    type Reply = EnsureNetworkReply;

    async fn handle(
        &mut self,
        msg: EnsureNetwork,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let actor_ref = ctx.actor_ref();
        match self.ensure_network(actor_ref, msg.network).await {
            Ok(()) => EnsureNetworkReply { error: None },
            Err(err) => EnsureNetworkReply {
                error: Some(format!("{err:#}")),
            },
        }
    }
}

impl Message<RemoveNetwork> for NetworkAgentActor {
    type Reply = RemoveNetworkReply;

    async fn handle(
        &mut self,
        msg: RemoveNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match self.remove_network(&msg.id).await {
            Ok(()) => RemoveNetworkReply { error: None },
            Err(err) => RemoveNetworkReply {
                error: Some(format!("{err:#}")),
            },
        }
    }
}

impl Message<LookupNetwork> for NetworkAgentActor {
    type Reply = LookupNetworkReply;

    async fn handle(
        &mut self,
        msg: LookupNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        LookupNetworkReply {
            network: self
                .networks
                .get(&msg.id)
                .map(|entry| entry.network.clone()),
        }
    }
}

impl Message<AttachNic> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: AttachNic,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.attach_nic(msg).await
    }
}

impl Message<DetachNic> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: DetachNic,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.detach_nic(msg).await
    }
}

impl Message<PublishPorts> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: PublishPorts,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Some(previous) = self.port_tasks.remove(&msg.vmid) {
            previous.abort();
            _ = previous.await;
        }
        if msg.ports.is_empty() {
            return Ok(());
        }

        match self.publish_target(msg.vmid, &msg.mac_address) {
            PublishTarget::Static(address) => {
                Self::publish_ports(msg.vmid, &msg.ports, address)?;
                info!(
                    vmid = %msg.vmid,
                    guest_ip = %address,
                    ports = msg.ports.len(),
                    "published ports"
                );
            }
            PublishTarget::Lease(lease_file) => {
                let vmid = msg.vmid;
                let mac_address = msg.mac_address.clone();
                let ports = msg.ports.clone();
                let task = tokio::spawn(async move {
                    match Self::wait_for_lease(&lease_file, &mac_address).await {
                        Ok(address) => match Self::publish_ports(vmid, &ports, address) {
                            Ok(()) => info!(
                                vmid = %vmid,
                                guest_ip = %address,
                                ports = ports.len(),
                                "published ports"
                            ),
                            Err(err) => error!(vmid = %vmid, ?err, "failed to publish ports"),
                        },
                        Err(err) => error!(vmid = %vmid, ?err, "failed to publish ports"),
                    }
                });
                self.port_tasks.insert(vmid, task);
            }
            PublishTarget::None => {
                warn!(
                    vmid = %msg.vmid,
                    mac = %msg.mac_address,
                    "no attached NIC matches the published ports; skipping"
                );
            }
        }

        Ok(())
    }
}

impl Message<UnpublishPorts> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: UnpublishPorts,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Some(task) = self.port_tasks.remove(&msg.vmid) {
            task.abort();
            _ = task.await;
        }
        Self::remove_port_rules(msg.vmid)
    }
}

impl Message<Status> for NetworkAgentActor {
    type Reply = NetworkStatus;

    async fn handle(&mut self, _msg: Status, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        NetworkStatus {
            networks: self
                .networks
                .iter()
                .map(|(id, entry)| NetworkSummary {
                    id: id.clone(),
                    provider: entry.network.provider,
                    members: entry
                        .members()
                        .iter()
                        .map(|member| AttachedNic {
                            vmid: member.vmid,
                            tap_name: member.tap_name.clone(),
                            address: member.address,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_network_maps_hostonly_nat_config() {
        let config = NetworkConfig {
            dhcp_config: Some(DhcpConfig {
                range: (
                    "10.10.100.100".parse().unwrap(),
                    "10.10.100.200".parse().unwrap(),
                ),
                subnet: "10.10.100.0/24".parse().unwrap(),
                lease_time: "12h".to_owned(),
            }),
            network_mode: NetworkMode::HostonlyNat {
                bridge: "vmbr0".to_owned(),
                subnet: "10.10.100.0/24".parse().unwrap(),
                gateway: "10.10.100.1".parse().unwrap(),
                upstream_iface: "enp4s0".to_owned(),
            },
        };

        let network = legacy_network(&config);
        network.validate().unwrap();
        assert_eq!(network.id, LEGACY_NETWORK_ID);
        assert_eq!(network.provider, NetworkProvider::Bridged);
        assert_eq!(network.bridge.as_deref(), Some("vmbr0"));
        assert_eq!(network.upstream.as_deref(), Some("enp4s0"));
        let dhcp = network.dhcp.unwrap();
        assert_eq!(dhcp.start, "10.10.100.100".parse::<Ipv4Addr>().unwrap());
        assert_eq!(dhcp.end, "10.10.100.200".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn legacy_network_maps_bridged_config() {
        let config = NetworkConfig {
            dhcp_config: None,
            network_mode: NetworkMode::Bridged {
                bridge: "vmbr1".to_owned(),
                subnet: "10.20.0.0/24".parse().unwrap(),
                gateway: "10.20.0.1".parse().unwrap(),
            },
        };

        let network = legacy_network(&config);
        network.validate().unwrap();
        assert_eq!(network.bridge.as_deref(), Some("vmbr1"));
        assert!(network.dhcp.is_none());
        assert!(network.upstream.is_none());
    }
}
