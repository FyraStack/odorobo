use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::config::{DhcpConfig, NetworkConfig, NetworkMode};
use crate::messages::network::{
    EnsureNetwork, EnsureNetworkReply, RemoveNetwork, RemoveNetworkReply,
};
use crate::network::{DhcpPool, Network, NetworkProvider};
use crate::networking::fabric::Fabric;
use crate::networking::messages::{
    AttachNic, AttachedNic, DetachNic, LookupNetwork, LookupNetworkReply, NetworkStatus,
    NetworkSummary, PublishPorts, Status, UnpublishPorts,
};
use crate::ports::{PortMapping, lease_ip};
use futures_util::{StreamExt, TryStreamExt};

use ipnet::Ipv4Net;
use kameo::{message::Context, prelude::*};
use nftables::{
    batch::Batch as NftBatch,
    expr::{Expression, Meta, MetaKey, NamedExpression, Payload, PayloadField},
    helper::{apply_ruleset, get_current_ruleset},
    schema::{Chain as NftChain, NfListObject, NfObject, Rule as NftRule, Table as NftTable},
    stmt::{Counter, Match as NftMatch, NAT, NATFamily, Operator, Statement},
    types::{NfChainPolicy, NfChainType, NfFamily, NfHook},
};
use rtnetlink::packet_route::{
    address::AddressAttribute,
    link::{
        AfSpecInet6, AfSpecUnspec, In6AddrGenMode, InfoData, InfoVrf, LinkAttribute, LinkInfo,
        LinkMessage,
    },
    route::{RouteMessage, RouteScope, RouteType},
};
use rtnetlink::{
    Error as NetlinkError, Handle, LinkBridge, LinkBridgePort, LinkMessageBuilder, LinkUnspec,
    LinkVrf, LinkVxlan, RouteMessageBuilder,
};
use stable_eyre::Report;
use stable_eyre::eyre::{Context as EyreContext, eyre};
use tokio::time::sleep;
use tracing::{error, info, warn};
use ulid::Ulid;

const NFT_TABLE: &str = "odorobo";
const NFT_DNAT_CHAIN: &str = "container_dnat";
const NFT_NIC_GUARD_CHAIN: &str = "nic_guard";
const NFT_GUEST_NAT_CHAIN: &str = "guest_nat";
const DNAT_PRIORITY: i32 = -100;
const ANTISPOOF_PRIORITY: i32 = -150;
const GUEST_NAT_PRIORITY: i32 = 100;
const VXLAN_PORT: u16 = 4789;
/// Tenant routing tables sit above the 24-bit VNI range.
const VRF_TABLE_BASE: u32 = 0x0100_0000;
/// Metric of a VRF's unreachable default, high enough for any learned default to win.
const VRF_UNREACHABLE_METRIC: u32 = 4_278_198_272;
/// How often the agent re-applies its FRR configuration, which FRR loses when it restarts.
const FABRIC_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
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

/// The bridge and VXLAN devices that carry a routed network across the EVPN fabric.
struct L3Vni {
    bridge: String,
    bridge_index: u32,
    vxlan: String,
    vxlan_index: u32,
}

struct RoutedState {
    vrf: String,
    vrf_index: u32,
    vni: u32,
    table_id: u32,
    gateway: Ipv4Addr,
    mtu: u16,
    l3vni: Option<L3Vni>,
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
    fn has_member(&self, vmid: Ulid, tap_name: &str) -> bool {
        self.members()
            .iter()
            .any(|member| member.vmid == vmid && member.tap_name == tap_name)
    }

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
        state: None,
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

/// Returns the index of link `name`, creating it from `link` when it is missing.
/// The one local IPv4 address inside `underlay`, which becomes this node's VTEP.
async fn underlay_address(handle: &Handle, underlay: Ipv4Net) -> Result<Ipv4Addr, Report> {
    let mut messages = handle.address().get().execute();
    let mut found = Vec::new();
    while let Some(message) = messages
        .try_next()
        .await
        .wrap_err("failed to list local addresses")?
    {
        for attribute in message.attributes {
            if let AddressAttribute::Local(IpAddr::V4(address)) = attribute
                && underlay.contains(&address)
                && !found.contains(&address)
            {
                found.push(address);
            }
        }
    }

    match found.as_slice() {
        [vtep] => Ok(*vtep),
        [] => Err(eyre!(
            "no local address is inside the EVPN underlay {underlay}"
        )),
        many => Err(eyre!(
            "several local addresses are inside the EVPN underlay {underlay}: {many:?}"
        )),
    }
}

fn vrf_table_id(link: &LinkMessage) -> Option<u32> {
    link.attributes.iter().find_map(|attribute| {
        let LinkAttribute::LinkInfo(info) = attribute else {
            return None;
        };
        info.iter().find_map(|info| {
            let LinkInfo::Data(InfoData::Vrf(data)) = info else {
                return None;
            };
            data.iter().find_map(|data| match data {
                InfoVrf::TableId(table_id) => Some(*table_id),
                _ => None,
            })
        })
    })
}

async fn ensure_link(handle: &Handle, name: &str, link: LinkMessage) -> Result<u32, Report> {
    if let Some(existing) = lookup_link(handle, name).await? {
        return Ok(existing.header.index);
    }

    info!(link = name, "link not found, creating it");
    handle
        .link()
        .add(link)
        .execute()
        .await
        .wrap_err_with(|| format!("failed to create link {name}"))?;

    let created = lookup_link(handle, name)
        .await?
        .ok_or_else(|| eyre!("link {name} was not found after creation"))?;
    Ok(created.header.index)
}

/// Without these, a lookup that misses the VRF table falls through to the main table.
fn unreachable_defaults(table_id: u32) -> [RouteMessage; 2] {
    [
        RouteMessageBuilder::<Ipv4Addr>::new()
            .table_id(table_id)
            .kind(RouteType::Unreachable)
            .priority(VRF_UNREACHABLE_METRIC)
            .build(),
        RouteMessageBuilder::<Ipv6Addr>::new()
            .table_id(table_id)
            .kind(RouteType::Unreachable)
            .priority(VRF_UNREACHABLE_METRIC)
            .build(),
    ]
}

async fn ensure_unreachable_default(handle: &Handle, table_id: u32) -> Result<(), Report> {
    for route in unreachable_defaults(table_id) {
        handle
            .route()
            .add(route)
            .replace()
            .execute()
            .await
            .wrap_err_with(|| {
                format!("failed to add the unreachable default to table {table_id}")
            })?;
    }
    Ok(())
}

/// Router MAC of this node's L3 VNI bridges: locally administered and derived from the VTEP.
fn router_mac(vtep: Ipv4Addr) -> Vec<u8> {
    let [a, b, c, d] = vtep.octets();
    vec![0x02, 0x00, a, b, c, d]
}

/// Creates or reconciles `vrfbr<vni>` in the VRF with `vrfvx<vni>` as its only port.
async fn ensure_l3vni(
    handle: &Handle,
    vni: u32,
    vrf_index: u32,
    vtep: Ipv4Addr,
    mtu: u16,
) -> Result<L3Vni, Report> {
    let bridge = format!("vrfbr{vni}");
    let bridge_index = ensure_link(
        handle,
        &bridge,
        LinkMessageBuilder::<LinkBridge>::new(&bridge).build(),
    )
    .await?;
    let no_link_local =
        LinkAttribute::AfSpecUnspec(vec![AfSpecUnspec::Inet6(vec![AfSpecInet6::AddrGenMode(
            In6AddrGenMode::None,
        )])]);
    handle
        .link()
        .change(
            LinkUnspec::new_with_index(bridge_index)
                .address(router_mac(vtep))
                .mtu(u32::from(mtu))
                .controller(vrf_index)
                .append_extra_attribute(no_link_local)
                .up()
                .build(),
        )
        .execute()
        .await
        .wrap_err_with(|| format!("failed to configure bridge {bridge}"))?;

    let vxlan = format!("vrfvx{vni}");
    let vxlan_index = ensure_link(
        handle,
        &vxlan,
        LinkVxlan::new(&vxlan, vni)
            .local(vtep)
            .port(VXLAN_PORT)
            .learning(false)
            .build(),
    )
    .await?;
    // Separate messages: the VXLAN changelink rejects IFLA_MTU.
    let vxlan_settings = [
        LinkMessageBuilder::<LinkVxlan>::new(&vxlan)
            .local(vtep)
            .build(),
        LinkUnspec::new_with_index(vxlan_index)
            .mtu(u32::from(mtu))
            .controller(bridge_index)
            .up()
            .build(),
        LinkBridgePort::new(vxlan_index)
            .learning(false)
            .neigh_suppress(true)
            .build(),
    ];
    for settings in vxlan_settings {
        handle
            .link()
            .change(settings)
            .execute()
            .await
            .wrap_err_with(|| format!("failed to configure vxlan {vxlan}"))?;
    }

    Ok(L3Vni {
        bridge,
        bridge_index,
        vxlan,
        vxlan_index,
    })
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
            AddressAttribute::Address(ip) | AddressAttribute::Local(ip) => {
                matches!(ip, IpAddr::V4(existing_ip) if *existing_ip == address)
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
    /// Set when the node is part of an EVPN fabric.
    fabric: Option<Fabric>,
    fabric_reconciler: Option<tokio::task::JoinHandle<()>>,
}

/// Re-applies this node's FRR configuration.
struct ReconcileFabric;

impl Actor for NetworkAgentActor {
    type Args = (NetworkConfig, PathBuf);
    type Error = Report;
    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let (config, runtime_root) = args;
        let (connection, handle, _) = rtnetlink::new_connection()?;
        let netlink_thread = tokio::spawn(connection);

        let fabric = match &config.evpn {
            Some(evpn) => {
                let vtep = underlay_address(&handle, evpn.underlay).await?;
                let fabric = Fabric::new(vtep, evpn.underlay, evpn.asn);
                fabric.ensure_bgp().await?;
                info!(%vtep, underlay = %evpn.underlay, asn = evpn.asn, "joined the EVPN fabric");
                Some(fabric)
            }
            None => None,
        };

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
            fabric_reconciler: fabric
                .is_some()
                .then(|| spawn_fabric_reconciler(&actor_ref)),
            fabric,
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
        if let Some(task) = self.fabric_reconciler.take() {
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
            if !existing.network.same_desired(&network) {
                return Err(eyre!(
                    "network {:?} already exists with different desired state",
                    network.id
                ));
            }
            if existing.network.vni() == network.vni() {
                return Ok(());
            }
            // The manager reassigns VNIs per process lifetime; migrate the
            // realization to the new VRF and re-attach the members.
            self.reassign_vni(&network).await?;
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

    /// Re-realizes a routed network under a newly assigned VNI.
    async fn reassign_vni(&mut self, network: &Network) -> Result<(), Report> {
        let Some(entry) = self.networks.remove(&network.id) else {
            return Err(eyre!("network {:?} is not realized", network.id));
        };
        let Realized::Routed(state) = entry.realized else {
            return Err(eyre!(
                "network {:?} is bridged; its vni cannot change",
                network.id
            ));
        };

        for member in &state.members {
            Self::detach_tap(&self.netlink_handle, &member.tap_name).await?;
        }
        if let Err(err) = self.delete_routed(&state).await {
            warn!(network_id = %network.id, vrf = %state.vrf, ?err, "old vrf not fully removed; realizing the new vni anyway");
        }

        let realized = Realized::Routed(self.realize_routed(network).await?);
        self.networks.insert(
            network.id.clone(),
            NetworkEntry {
                network: network.clone(),
                realized,
            },
        );

        self.reattach_members(&network.id, &state.members).await?;

        info!(
            network_id = %network.id,
            vni = ?network.vni(),
            "reassigned network vni"
        );
        Ok(())
    }

    /// Detaches one tap, tolerating a tap that is already gone.
    async fn detach_tap(handle: &Handle, tap_name: &str) -> Result<(), Report> {
        Self::remove_antispoof_rules(tap_name)?;
        if let Some(tap) = lookup_link(handle, tap_name).await? {
            handle
                .link()
                .set(
                    LinkUnspec::new_with_index(tap.header.index)
                        .nocontroller()
                        .build(),
                )
                .execute()
                .await
                .wrap_err_with(|| format!("failed to detach tap {tap_name}"))?;
        }
        Ok(())
    }

    /// Withdraws the network from FRR and deletes its devices; taps are released, not deleted.
    async fn delete_routed(&self, state: &RoutedState) -> Result<(), Report> {
        let handle = &self.netlink_handle;
        let mut first_error = match &self.fabric {
            Some(fabric) => fabric.withdraw_network(&state.vrf, state.vni).await.err(),
            None => None,
        };
        if let Some(err) = &first_error {
            error!(vrf = %state.vrf, ?err, "failed to remove the network from FRR");
        }

        let l3vni_links = state.l3vni.iter().flat_map(|l3vni| {
            [
                (l3vni.vxlan_index, &l3vni.vxlan),
                (l3vni.bridge_index, &l3vni.bridge),
            ]
        });
        for (index, name) in l3vni_links.chain([(state.vrf_index, &state.vrf)]) {
            match handle.link().del(index).execute().await {
                Err(NetlinkError::NetlinkError(err)) if err.raw_code() == -libc::ENODEV => {}
                Err(err) => {
                    error!(link = %name, %err, "failed to delete link");
                    first_error.get_or_insert_with(|| eyre!("failed to delete link {name}: {err}"));
                }
                Ok(()) => {}
            }
        }
        // The defaults sit on lo, so deleting the VRF leaves them in its table.
        for route in unreachable_defaults(state.table_id) {
            match handle.route().del(route).execute().await {
                Err(NetlinkError::NetlinkError(err)) if err.raw_code() == -libc::ESRCH => {}
                Err(err) => {
                    error!(table = state.table_id, %err, "failed to delete the unreachable default");
                    first_error.get_or_insert_with(|| {
                        eyre!(
                            "failed to delete the unreachable default from table {}: {err}",
                            state.table_id
                        )
                    });
                }
                Ok(()) => {}
            }
        }
        if let Some(fabric) = &self.fabric
            && let Err(err) = fabric.forget_vrf(&state.vrf).await
        {
            error!(vrf = %state.vrf, ?err, "failed to remove the VRF from FRR");
            first_error.get_or_insert(err);
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Re-attaches recorded members, skipping taps that no longer exist.
    async fn reattach_members(
        &mut self,
        network_id: &str,
        members: &[Member],
    ) -> Result<(), Report> {
        for member in members {
            if lookup_link(&self.netlink_handle, &member.tap_name)
                .await?
                .is_none()
            {
                warn!(
                    vmid = %member.vmid,
                    tap = %member.tap_name,
                    "tap is gone; not re-attaching it to the new vrf"
                );
                continue;
            }
            self.attach_nic(AttachNic {
                vmid: member.vmid,
                network_id: network_id.to_owned(),
                tap_name: member.tap_name.clone(),
                mac_address: member.mac_address.clone(),
                address: member.address,
            })
            .await?;
        }
        Ok(())
    }

    async fn realize_routed(&self, network: &Network) -> Result<RoutedState, Report> {
        let vni = network
            .vni()
            .ok_or_else(|| eyre!("routed network {:?} requires a vni", network.id))?;
        let table_id = VRF_TABLE_BASE.saturating_add(vni);
        let vrf = format!("vrf{vni}");
        if let Some(link) = lookup_link(&self.netlink_handle, &vrf).await?
            && vrf_table_id(&link) != Some(table_id)
        {
            return Err(eyre!(
                "vrf device {vrf} does not use routing table {table_id}; remove it before retrying"
            ));
        }
        let vrf_index = ensure_link(
            &self.netlink_handle,
            &vrf,
            LinkVrf::new(&vrf, table_id).up().build(),
        )
        .await?;
        ensure_unreachable_default(&self.netlink_handle, table_id).await?;

        let l3vni = match &self.fabric {
            Some(fabric) => {
                let l3vni = ensure_l3vni(
                    &self.netlink_handle,
                    vni,
                    vrf_index,
                    fabric.vtep,
                    network.mtu(),
                )
                .await?;
                fabric.ensure_network(&vrf, vni).await?;
                Some(l3vni)
            }
            None => None,
        };

        let state = RoutedState {
            vrf,
            vrf_index,
            vni,
            table_id,
            gateway: network.gateway(),
            mtu: network.mtu(),
            l3vni,
            members: Vec::new(),
        };

        info!(
            network_id = %network.id,
            vrf = %state.vrf,
            vni,
            table = table_id,
            gateway = %state.gateway,
            vxlan = ?state.l3vni.as_ref().map(|l3vni| &l3vni.vxlan),
            "routed network realized"
        );
        Ok(state)
    }

    async fn realize_bridged(
        &self,
        actor_ref: &ActorRef<Self>,
        network: &Network,
    ) -> Result<BridgedState, Report> {
        let bridge = self.prepare_bridge(network).await?;
        let lease_file = self.lease_file_for(&network.id);
        if let Some(upstream) = &network.upstream {
            Self::ensure_nat_rules(upstream)?;
        }
        let dhcp_actor = Self::start_dhcp(actor_ref, network, &bridge, &lease_file).await;

        Ok(BridgedState {
            bridge,
            dhcp_actor,
            lease_file,
            members: Vec::new(),
        })
    }

    async fn prepare_bridge(&self, network: &Network) -> Result<String, Report> {
        let bridge = network
            .bridge
            .clone()
            .ok_or_else(|| eyre!("bridged network {:?} requires a bridge", network.id))?;
        let bridge_index = ensure_link(
            &self.netlink_handle,
            &bridge,
            LinkBridge::new(&bridge).up().build(),
        )
        .await?;
        ensure_address(
            &self.netlink_handle,
            bridge_index,
            &bridge,
            network.gateway(),
            network.subnet.prefix_len(),
        )
        .await?;
        Ok(bridge)
    }

    async fn start_dhcp(
        actor_ref: &ActorRef<Self>,
        network: &Network,
        bridge: &str,
        lease_file: &Path,
    ) -> Option<ActorRef<DhcpActor>> {
        let dhcp = network.dhcp.as_ref()?;
        let config = DhcpConfig {
            range: (dhcp.start, dhcp.end),
            subnet: network.subnet,
            lease_time: dhcp.lease_time.clone(),
        };
        Some(
            DhcpActor::spawn_link(
                actor_ref,
                (config, bridge.to_owned(), lease_file.to_owned()),
            )
            .await,
        )
    }

    async fn remove_network(&mut self, id: &str) -> Result<(), Report> {
        let Some(entry) = self.networks.get(id) else {
            return Err(eyre!("unknown network {id}"));
        };
        if !entry.members().is_empty() {
            return Err(eyre!("network {id} still has attached NICs"));
        }
        // Keep the entry until cleanup succeeds so a failed removal can be retried.
        if let Realized::Routed(state) = &entry.realized {
            self.delete_routed(state).await?;
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
                info!(network_id = %id, vrf = %state.vrf, "removed routed network");
            }
        }

        Ok(())
    }

    async fn attach_nic(&mut self, msg: AttachNic) -> Result<(), Report> {
        let Some(entry) = self.networks.get_mut(&msg.network_id) else {
            return Err(eyre!("unknown network {}", msg.network_id));
        };
        if entry.has_member(msg.vmid, &msg.tap_name) {
            info!(
                vmid = %msg.vmid,
                tap = %msg.tap_name,
                network_id = %msg.network_id,
                "NIC already attached"
            );
            return Ok(());
        }

        Self::attach_to_network(&self.netlink_handle, entry, &msg).await?;

        info!(
            vmid = %msg.vmid,
            tap = %msg.tap_name,
            network_id = %msg.network_id,
            "attached NIC"
        );
        Ok(())
    }

    async fn attach_to_network(
        handle: &Handle,
        entry: &mut NetworkEntry,
        msg: &AttachNic,
    ) -> Result<(), Report> {
        let address = match &entry.realized {
            Realized::Routed(state) => {
                Self::attach_routed(handle, state, msg).await?;
                msg.address
            }
            Realized::Bridged(state) => {
                Self::attach_bridged(handle, state, msg).await?;
                None
            }
        };
        entry.members_mut().push(Member {
            vmid: msg.vmid,
            tap_name: msg.tap_name.clone(),
            mac_address: msg.mac_address.clone(),
            address,
        });
        Ok(())
    }

    async fn attach_routed(
        handle: &Handle,
        state: &RoutedState,
        msg: &AttachNic,
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

        Self::configure_tap(handle, tap_index, msg, state.vrf_index, Some(state.mtu))
            .await
            .wrap_err_with(|| {
                format!("failed to attach tap {} to vrf {}", msg.tap_name, state.vrf)
            })?;
        ensure_address(handle, tap_index, &msg.tap_name, state.gateway, 32).await?;
        Self::add_vm_route(handle, tap_index, &msg.tap_name, address, state.table_id).await?;
        write_tap_sysctls(&msg.tap_name)?;
        Self::ensure_antispoof_rule(&msg.tap_name, address)?;
        Ok(())
    }

    async fn attach_bridged(
        handle: &Handle,
        state: &BridgedState,
        msg: &AttachNic,
    ) -> Result<(), Report> {
        let bridge = lookup_link(handle, &state.bridge)
            .await?
            .ok_or_else(|| eyre!("bridge {} not found", state.bridge))?;
        let tap = lookup_link(handle, &msg.tap_name)
            .await?
            .ok_or_else(|| eyre!("tap {} not found", msg.tap_name))?;

        Self::configure_tap(handle, tap.header.index, msg, bridge.header.index, None)
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to attach tap {} to bridge {}",
                    msg.tap_name, state.bridge
                )
            })?;
        Ok(())
    }

    async fn configure_tap(
        handle: &Handle,
        tap_index: u32,
        msg: &AttachNic,
        controller_index: u32,
        mtu: Option<u16>,
    ) -> Result<(), NetlinkError> {
        let mut link = LinkUnspec::new_with_index(tap_index)
            .alias(format!("odorobo {} {}", msg.vmid, msg.network_id))
            .controller(controller_index)
            .up();
        if let Some(mtu) = mtu {
            link = link.mtu(u32::from(mtu));
        }
        handle.link().set(link.build()).execute().await
    }

    async fn add_vm_route(
        handle: &Handle,
        tap_index: u32,
        tap_name: &str,
        address: Ipv4Addr,
        table_id: u32,
    ) -> Result<(), Report> {
        handle
            .route()
            .add(
                RouteMessageBuilder::<Ipv4Addr>::new()
                    .table_id(table_id)
                    .output_interface(tap_index)
                    .scope(RouteScope::Link)
                    .destination_prefix(address, 32)
                    .build(),
            )
            .execute()
            .await
            .wrap_err_with(|| {
                format!("failed to add route {address}/32 via tap {tap_name} to table {table_id}")
            })?;
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

        Self::detach_tap(&self.netlink_handle, &member.tap_name).await?;

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
        objects.iter().any(|object| {
            let NfObject::ListObject(NfListObject::Rule(existing)) = object else {
                return false;
            };
            existing.family == NfFamily::IP
                && existing.table.as_ref() == table
                && existing.chain.as_ref() == chain
                && Self::is_upstream_masquerade(existing.expr.as_ref(), upstream_iface)
        })
    }

    fn is_upstream_masquerade(expr: &[Statement<'_>], upstream_iface: &str) -> bool {
        let [Statement::Match(condition), Statement::Masquerade(_)] = expr else {
            return false;
        };
        let Expression::Named(NamedExpression::Meta(meta)) = &condition.left else {
            return false;
        };
        let Expression::String(iface) = &condition.right else {
            return false;
        };
        condition.op == Operator::EQ
            && meta.key == MetaKey::Oifname
            && iface.as_ref() == upstream_iface
    }

    /// Ensures the host-only NAT masquerade rule exists for the configured
    /// upstream interface.
    ///
    /// This is intentionally IPv4-only for now: guest subnet config uses
    /// `Ipv4Net`, bridge gateways use `Ipv4Addr`, and switching the masquerade
    /// rule to dual-stack would implicitly opt into IPv6 NAT policy.
    fn ensure_nat_rules(upstream_iface: &str) -> Result<(), Report> {
        let ruleset = get_current_ruleset().wrap_err("failed to fetch current nftables ruleset")?;
        if Self::nft_postrouting_masquerade_exists(
            ruleset.objects.as_ref(),
            NFT_TABLE,
            NFT_GUEST_NAT_CHAIN,
            upstream_iface,
        ) {
            info!(
                upstream_iface = upstream_iface,
                "existing NAT masquerade rule already present"
            );
            return Ok(());
        }

        Self::ensure_base_chain(
            NFT_GUEST_NAT_CHAIN,
            NfChainType::NAT,
            NfHook::Postrouting,
            GUEST_NAT_PRIORITY,
        )?;

        let mut batch = NftBatch::new();
        batch.add(NfListObject::Rule(Self::masquerade_rule(upstream_iface)));
        apply_ruleset(&batch.to_nftables()).wrap_err_with(|| {
            format!(
                "failed to apply nftables postrouting masquerade for upstream interface {upstream_iface}"
            )
        })?;

        Ok(())
    }

    /// Build NAT masquerade rule for the given upstream interface.
    fn masquerade_rule(upstream_iface: &str) -> NftRule<'static> {
        NftRule {
            family: NfFamily::IP,
            table: NFT_TABLE.into(),
            chain: NFT_GUEST_NAT_CHAIN.into(),
            expr: vec![
                Statement::Match(NftMatch {
                    left: Expression::Named(NamedExpression::Meta(Meta {
                        key: MetaKey::Oifname,
                    })),
                    right: Expression::String(upstream_iface.to_owned().into()),
                    op: Operator::EQ,
                }),
                Statement::Masquerade(None),
            ]
            .into(),
            handle: None,
            index: None,
            comment: Some("odorobo host-only NAT".into()),
        }
    }

    /// Ensures the shared `odorobo` table and a base chain exist.
    fn ensure_base_chain(
        chain: &str,
        chain_type: NfChainType,
        hook: NfHook,
        priority: i32,
    ) -> Result<(), Report> {
        let ruleset = get_current_ruleset().wrap_err("failed to fetch current nftables ruleset")?;
        let objects = ruleset.objects.as_ref();
        if Self::nft_chain_exists(objects, NFT_TABLE, chain) {
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
            name: chain.into(),
            newname: None,
            handle: None,
            _type: Some(chain_type),
            hook: Some(hook),
            prio: Some(priority),
            dev: None,
            policy: Some(NfChainPolicy::Accept),
        }));
        apply_ruleset(&batch.to_nftables())
            .wrap_err_with(|| format!("failed to create the {chain} chain"))?;
        info!(table = NFT_TABLE, chain = chain, "created nftables chain");
        Ok(())
    }

    /// Yields the rules of one `odorobo` chain.
    fn rules_in_chain<'a>(
        objects: &'a [NfObject<'static>],
        chain: &str,
    ) -> impl Iterator<Item = &'a NftRule<'static>> {
        objects.iter().filter_map(move |object| {
            let NfObject::ListObject(NfListObject::Rule(rule)) = object else {
                return None;
            };
            (rule.family == NfFamily::IP
                && rule.table.as_ref() == NFT_TABLE
                && rule.chain.as_ref() == chain)
                .then_some(rule)
        })
    }

    fn delete_nft_rule(batch: &mut NftBatch<'static>, rule: &NftRule<'static>) -> bool {
        let Some(handle) = rule.handle else {
            return false;
        };
        batch.delete(NfListObject::Rule(NftRule {
            family: rule.family,
            table: rule.table.clone(),
            chain: rule.chain.clone(),
            expr: Vec::new().into(),
            handle: Some(handle),
            index: None,
            comment: None,
        }));
        true
    }

    /// Deletes anti-spoof rules whose TAP no longer exists (rules outlive process restarts).
    async fn prune_stale_antispoof_rules(handle: &Handle) -> Result<(), Report> {
        let ruleset = get_current_ruleset()
            .wrap_err("failed to fetch nftables ruleset for stale anti-spoof rules")?;
        let mut batch = NftBatch::new();
        for rule in Self::rules_in_chain(ruleset.objects.as_ref(), NFT_NIC_GUARD_CHAIN) {
            let Some(tap_name) = rule
                .comment
                .as_deref()
                .and_then(|comment| comment.strip_prefix("odorobo antispoof "))
                .and_then(|rest| rest.split_whitespace().next())
            else {
                continue;
            };
            if lookup_link(handle, tap_name).await?.is_none()
                && Self::delete_nft_rule(&mut batch, rule)
            {
                info!(tap = tap_name, "pruned stale anti-spoof rule");
            }
        }
        apply_ruleset(&batch.to_nftables()).wrap_err("failed to prune stale anti-spoof rules")
    }

    fn match_incoming_tap(tap_name: &str) -> Statement<'static> {
        Statement::Match(NftMatch {
            left: Expression::Named(NamedExpression::Meta(Meta {
                key: MetaKey::Iifname,
            })),
            right: Expression::String(tap_name.to_owned().into()),
            op: Operator::EQ,
        })
    }

    fn match_spoofed_ipv4_source(address: Ipv4Addr) -> Statement<'static> {
        let source = PayloadField {
            protocol: "ip".into(),
            field: "saddr".into(),
        };
        Statement::Match(NftMatch {
            left: Expression::Named(NamedExpression::Payload(Payload::PayloadField(source))),
            right: Expression::String(address.to_string().into()),
            op: Operator::NEQ,
        })
    }

    fn antispoof_rule(tap_name: &str, address: Ipv4Addr) -> NftRule<'static> {
        NftRule {
            family: NfFamily::IP,
            table: NFT_TABLE.into(),
            chain: NFT_NIC_GUARD_CHAIN.into(),
            expr: vec![
                Self::match_incoming_tap(tap_name),
                Self::match_spoofed_ipv4_source(address),
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
        Self::ensure_base_chain(
            NFT_NIC_GUARD_CHAIN,
            NfChainType::Filter,
            NfHook::Prerouting,
            ANTISPOOF_PRIORITY,
        )?;

        let ruleset =
            get_current_ruleset().wrap_err("failed to fetch nftables ruleset for NIC guard")?;
        let prefix = format!("odorobo antispoof {tap_name} ");
        let expected = format!("odorobo antispoof {tap_name} {address}");
        let mut batch = NftBatch::new();
        let mut present = false;
        for rule in Self::rules_in_chain(ruleset.objects.as_ref(), NFT_NIC_GUARD_CHAIN) {
            let Some(comment) = rule.comment.as_deref() else {
                continue;
            };
            if comment == expected {
                present = true;
            } else if comment.starts_with(&prefix) {
                Self::delete_nft_rule(&mut batch, rule);
            }
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
        for rule in Self::rules_in_chain(ruleset.objects.as_ref(), NFT_NIC_GUARD_CHAIN) {
            if rule
                .comment
                .as_deref()
                .is_some_and(|comment| comment.starts_with(&prefix))
                && Self::delete_nft_rule(&mut batch, rule)
            {
                removed = removed.saturating_add(1);
            }
        }
        if removed > 0 {
            apply_ruleset(&batch.to_nftables())
                .wrap_err_with(|| format!("failed to remove anti-spoof rules for {tap_name}"))?;
            info!(tap = tap_name, rules = removed, "removed anti-spoof rules");
        }
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
        Self::ensure_base_chain(
            NFT_DNAT_CHAIN,
            NfChainType::NAT,
            NfHook::Prerouting,
            DNAT_PRIORITY,
        )?;

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
        for rule in Self::rules_in_chain(ruleset.objects.as_ref(), NFT_DNAT_CHAIN) {
            if Self::delete_nft_rule(&mut batch, rule) {
                removed = removed.saturating_add(1);
            }
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
        for rule in Self::rules_in_chain(ruleset.objects.as_ref(), NFT_DNAT_CHAIN) {
            if rule
                .comment
                .as_deref()
                .is_some_and(|comment| comment.starts_with(&prefix))
                && Self::delete_nft_rule(&mut batch, rule)
            {
                removed = removed.saturating_add(1);
            }
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

fn spawn_fabric_reconciler(actor_ref: &ActorRef<NetworkAgentActor>) -> tokio::task::JoinHandle<()> {
    let actor_ref = actor_ref.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(FABRIC_RECONCILE_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            if actor_ref.tell(ReconcileFabric).send().await.is_err() {
                return;
            }
        }
    })
}

impl Message<ReconcileFabric> for NetworkAgentActor {
    type Reply = ();

    async fn handle(&mut self, _msg: ReconcileFabric, _ctx: &mut Context<Self, Self::Reply>) {
        let Some(fabric) = &self.fabric else {
            return;
        };
        let routed: Vec<(&str, u32)> = self
            .networks
            .values()
            .filter_map(|entry| match &entry.realized {
                Realized::Routed(state) => Some((state.vrf.as_str(), state.vni)),
                Realized::Bridged(_) => None,
            })
            .collect();
        if let Err(err) = fabric.reconcile(&routed).await {
            warn!(?err, "failed to reconcile the FRR configuration");
        }
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
    use crate::network::{NetworkState, VNI_RANGE};
    use rtnetlink::packet_route::route::RouteAttribute;
    use std::net::Ipv6Addr;

    async fn routes_in_table(handle: &Handle, table_id: u32) -> Result<Vec<RouteMessage>, Report> {
        let mut routes = Vec::new();
        for query in [
            RouteMessageBuilder::<Ipv4Addr>::new().build(),
            RouteMessageBuilder::<Ipv6Addr>::new().build(),
        ] {
            let mut stream = handle.route().get(query).execute();
            while let Some(route) = stream.try_next().await? {
                let table = route
                    .attributes
                    .iter()
                    .find_map(|attribute| match attribute {
                        RouteAttribute::Table(table) => Some(*table),
                        _ => None,
                    })
                    .unwrap_or_else(|| u32::from(route.header.table));
                if table == table_id {
                    routes.push(route);
                }
            }
        }
        Ok(routes)
    }

    #[tokio::test]
    #[ignore = "requires root and an isolated network namespace"]
    async fn routed_vnis_do_not_reuse_host_tables() -> Result<(), Report> {
        let (connection, handle, _) = rtnetlink::new_connection()?;
        let actor = NetworkAgentActor {
            config: NetworkConfig::default(),
            runtime_root: PathBuf::new(),
            networks: HashMap::new(),
            port_tasks: HashMap::new(),
            netlink_thread: tokio::spawn(connection),
            netlink_handle: handle,
            fabric_reconciler: None,
            fabric: None,
        };
        let handle = &actor.netlink_handle;
        let mut network = Network {
            id: "table-test".to_owned(),
            subnet: "10.40.10.0/24".parse()?,
            gateway: None,
            provider: NetworkProvider::Routed,
            state: Some(NetworkState { vni: 52 }),
            mtu: None,
            bridge: None,
            dhcp: None,
            upstream: None,
        };

        ensure_unreachable_default(handle, 52).await?;
        let host_routes = routes_in_table(handle, 52).await?;
        assert_eq!(host_routes.len(), 2);
        let host_vrf = ensure_link(handle, "vrf52", LinkVrf::new("vrf52", 52).up().build()).await?;
        assert!(actor.realize_routed(&network).await.is_err());
        let host_link = lookup_link(handle, "vrf52")
            .await?
            .ok_or_else(|| eyre!("host VRF disappeared"))?;
        assert_eq!(vrf_table_id(&host_link), Some(52));
        handle.link().del(host_vrf).execute().await?;

        for vni in [52, 252, *VNI_RANGE.end()] {
            network.state = Some(NetworkState { vni });
            let mut state = actor.realize_routed(&network).await?;
            let link = lookup_link(handle, &state.vrf)
                .await?
                .ok_or_else(|| eyre!("tenant VRF disappeared"))?;
            assert_eq!(
                vrf_table_id(&link),
                Some(VRF_TABLE_BASE.saturating_add(vni))
            );
            assert_ne!(state.table_id, vni);
            assert_eq!(
                actor.realize_routed(&network).await?.vrf_index,
                state.vrf_index
            );

            state.l3vni = Some(
                ensure_l3vni(
                    handle,
                    vni,
                    state.vrf_index,
                    "192.0.2.1".parse()?,
                    state.mtu,
                )
                .await?,
            );
            actor.delete_routed(&state).await?;
            assert!(lookup_link(handle, &state.vrf).await?.is_none());
            assert_eq!(
                routes_in_table(handle, state.table_id).await?,
                Vec::<RouteMessage>::new()
            );
            assert_eq!(routes_in_table(handle, 52).await?, host_routes);
        }

        actor.netlink_thread.abort();
        _ = actor.netlink_thread.await;
        Ok(())
    }

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
            evpn: None,
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
            evpn: None,
        };

        let network = legacy_network(&config);
        network.validate().unwrap();
        assert_eq!(network.bridge.as_deref(), Some("vmbr1"));
        assert!(network.dhcp.is_none());
        assert!(network.upstream.is_none());
    }
}
