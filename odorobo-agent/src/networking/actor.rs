use crate::actor::{DhcpConfig, NetworkConfig, NetworkMode};
use crate::networking::messages::{AttachTap, DetachTap};
use futures_util::{StreamExt, TryStreamExt};

use kameo::{message::Context, prelude::*};
use nftnl::{
    Batch, FinalizedBatch, Hook, MsgType, ProtoFamily, Rule, Table, expr::InterfaceName, nft_expr,
};
use rtnetlink::{Error as NetlinkError, Handle, LinkBridge, LinkUnspec};
use stable_eyre::Report;
use stable_eyre::eyre::{Context as EyreContext, eyre};
use std::ffi::CString;
use std::process::Command;
use tracing::info;

pub struct DhcpActor {
    pub config: DhcpConfig,
    bridge: String,
    dnsmasq_process: Option<tokio::process::Child>,
}

pub struct NetworkConfigCommon {
    pub bridge: String,
    pub subnet: String,
    pub upstream_iface: Option<String>,
}

impl Actor for DhcpActor {
    type Args = (DhcpConfig, String);
    type Error = Report;
    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let (config, bridge) = args;
        let dhcp_range = format!(
            "{},{},{},{}",
            config.range.0,
            config.range.1,
            config.subnet.netmask(),
            config.lease_time
        );

        info!(
            bridge = %bridge,
            range_start = %config.range.0,
            range_end = %config.range.1,
            subnet = %config.subnet,
            lease_time = %config.lease_time,
            "starting dnsmasq for guest DHCP"
        );

        let dnsmasq_process = tokio::process::Command::new("dnsmasq")
            .arg("--interface")
            .arg(&bridge)
            .arg("--bind-interfaces")
            .arg("--dhcp-range")
            .arg(&dhcp_range)
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

            let _ = dnsmasq_process.wait().await;
        }

        Ok(())
    }
}

async fn ensure_address(
    handle: &Handle,
    link_index: u32,
    link_name: &str,
    address: std::net::Ipv4Addr,
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
                bridge = link_name,
                address = %address,
                prefix_len,
                "bridge gateway address already present"
            );
            return Ok(());
        }
    }

    info!(
        bridge = link_name,
        address = %address,
        prefix_len,
        "adding gateway address to bridge"
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

#[derive(RemoteActor)]
pub struct NetworkAgentActor {
    pub config: NetworkConfig,
    common: NetworkConfigCommon,
    pub dhcp_actor: Option<ActorRef<DhcpActor>>,
    // netlink_handle:
    netlink_thread: tokio::task::JoinHandle<()>,
    netlink_handle: Handle,
}

impl NetworkAgentActor {
    async fn lookup_link_by_name(
        &self,
        link_name: &str,
    ) -> Result<rtnetlink::packet_route::link::LinkMessage, Report> {
        self.netlink_handle
            .link()
            .get()
            .match_name(link_name.to_string())
            .execute()
            .next()
            .await
            .ok_or_else(|| eyre!("link {} not found", link_name))?
            .wrap_err_with(|| format!("failed to query link {}", link_name))
    }

    fn send_nft_batch(batch: &FinalizedBatch) -> Result<(), Report> {
        let socket = mnl::Socket::new(mnl::Bus::Netfilter)
            .wrap_err("failed to create netfilter netlink socket")?;
        let portid = socket.portid();

        info!("sending nftables batch to netfilter");

        socket
            .send_all(batch)
            .wrap_err("failed to send nftables batch to netfilter")?;

        let mut buffer = vec![0; nftnl::nft_nlmsg_maxsize() as usize];
        let mut expected_seqs = batch.sequence_numbers();

        while !expected_seqs.is_empty() {
            for message in socket
                .recv(&mut buffer[..])
                .wrap_err("failed to receive nftables netlink acknowledgement")?
            {
                let message = message.wrap_err("failed to decode nft ack message")?;
                let expected_seq = expected_seqs
                    .next()
                    .ok_or_else(|| eyre!("received unexpected nftables acknowledgement"))?;

                mnl::cb_run(message, expected_seq, portid)
                    .wrap_err("nftables batch acknowledgement failed")?;
            }
        }

        Ok(())
    }

    // use the nft CLI instead because doing full introspection is kind of a pain
    fn nft_table_exists(table: &str) -> Result<bool, Report> {
        let output = Command::new("nft")
            .args(["list", "table", "ip", table])
            .output()
            .wrap_err_with(|| format!("failed to query nft table ip {table}"))?;

        Ok(output.status.success())
    }

    fn nft_chain_exists(table: &str, chain: &str) -> Result<bool, Report> {
        let output = Command::new("nft")
            .args(["list", "chain", "ip", table, chain])
            .output()
            .wrap_err_with(|| format!("failed to query nft chain ip {table} {chain}"))?;

        Ok(output.status.success())
    }

    fn nft_postrouting_masquerade_exists(
        table: &str,
        chain: &str,
        upstream_iface: &str,
    ) -> Result<bool, Report> {
        let output = Command::new("nft")
            .args(["list", "chain", "ip", table, chain])
            .output()
            .wrap_err_with(|| format!("failed to inspect nft chain ip {table} {chain}"))?;

        if !output.status.success() {
            return Ok(false);
        }

        let stdout = String::from_utf8(output.stdout)
            .wrap_err_with(|| format!("failed to decode nft output for ip {table} {chain}"))?;

        Ok(stdout.contains(&format!("oifname \"{upstream_iface}\" masquerade")))
    }

    /// Ensures the host-only NAT masquerade rule exists for the configured
    /// upstream interface.
    ///
    /// This is intentionally IPv4-only for now.
    ///
    /// The current host-only networking model is built around IPv4 guest
    /// addressing and an IPv4 bridge gateway:
    /// - guest subnet config uses `Ipv4Net`
    /// - bridge gateway config uses `Ipv4Addr`
    /// - bridge address assignment is currently IPv4-only
    ///
    /// Although nftables can match by interface in `inet` tables, switching
    /// this masquerade rule to dual-stack would implicitly opt us into IPv6
    /// NAT/NAT66 policy as well. That is a larger design decision than this
    /// hook should make on its own. Until odorobo has an intentional IPv6
    /// guest-networking story, we keep host-only NAT scoped to IPv4.
    ///
    // todo: IPv6, refer to libvirt's impl:
    // ```nft
    // table ip6 libvirt_network {
    //         chain forward {
    //                 type filter hook forward priority filter; policy accept;
    //                 counter packets 0 bytes 0 jump guest_cross
    //                 counter packets 0 bytes 0 jump guest_input
    //                 counter packets 0 bytes 0 jump guest_output
    //         }

    //         chain guest_output {
    //         }

    //         chain guest_input {
    //         }

    //         chain guest_cross {
    //         }

    //         chain guest_nat {
    //                 type nat hook postrouting priority srcnat; policy accept;
    //         }
    // }
    // ```

    fn ensure_nat_rules(_bridge: &str, _subnet: &str, upstream_iface: &str) -> Result<(), Report> {
        const TABLE_NAME: &str = "odorobo";
        const CHAIN_NAME: &str = "guest_nat";

        info!(
            table = TABLE_NAME,
            chain = CHAIN_NAME,
            upstream_iface = upstream_iface,
            "ensuring host-only NAT masquerade rule exists"
        );

        let table_exists = Self::nft_table_exists(TABLE_NAME)?;
        let chain_exists = if table_exists {
            Self::nft_chain_exists(TABLE_NAME, CHAIN_NAME)?
        } else {
            false
        };

        if chain_exists
            && Self::nft_postrouting_masquerade_exists(TABLE_NAME, CHAIN_NAME, upstream_iface)?
        {
            info!(
                table = TABLE_NAME,
                chain = CHAIN_NAME,
                upstream_iface = upstream_iface,
                "existing NAT masquerade rule already present"
            );
            return Ok(());
        }

        let table = Table::new(c"odorobo", ProtoFamily::Ipv4);

        let mut postrouting_chain = nftnl::Chain::new(c"guest_nat", &table);
        postrouting_chain.set_type(nftnl::ChainType::Nat);
        postrouting_chain.set_hook(Hook::PostRouting, 100);

        let mut batch = Batch::new();
        if !table_exists {
            info!(table = TABLE_NAME, "creating odorobo nftables table");
            batch.add(&table, MsgType::Add);
        }
        if !chain_exists {
            info!(
                table = TABLE_NAME,
                chain = CHAIN_NAME,
                "creating odorobo nftables chain"
            );
            batch.add(&postrouting_chain, MsgType::Add);
        }

        let mut postrouting_rule = Rule::new(&postrouting_chain);
        let upstream_iface = InterfaceName::Exact(
            CString::new(upstream_iface)
                .wrap_err("upstream interface name contained interior NUL")?,
        );
        postrouting_rule.add_expr(&nft_expr!(meta oifname));
        postrouting_rule.add_expr(&nft_expr!(cmp == &upstream_iface));
        postrouting_rule.add_expr(&nft_expr!(masquerade));
        batch.add(&postrouting_rule, MsgType::Add);

        let finalized = batch.finalize();
        Self::send_nft_batch(&finalized).wrap_err_with(|| {
            format!("failed to apply nftables postrouting masquerade for {upstream_iface:?}")
        })?;

        Ok(())
    }
}

impl Actor for NetworkAgentActor {
    type Args = NetworkConfig;
    type Error = Report;
    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // do some netlink fuckery here

        let (connection, handle, _) = rtnetlink::new_connection()?;
        let netlink_thread = tokio::spawn(connection);

        let common = match args.network_mode.clone() {
            NetworkMode::HostonlyNat {
                bridge,
                subnet,
                gateway: _,
                upstream_iface,
            } => NetworkConfigCommon {
                bridge,
                subnet: subnet.to_string(),
                upstream_iface: Some(upstream_iface),
            },
            NetworkMode::Bridged {
                bridge,
                subnet,
                gateway: _,
            } => NetworkConfigCommon {
                bridge,
                subnet: subnet.to_string(),
                upstream_iface: None,
            },
        };

        // ensure the bridge exists, creating it if necessary
        let bridge_lookup = handle
            .link()
            .get()
            .match_name(common.bridge.clone())
            .execute()
            .next()
            .await;

        let bridge = match bridge_lookup {
            Some(Ok(bridge)) => bridge,
            Some(Err(err)) if matches!(err, NetlinkError::NetlinkError(ref nl_err) if nl_err.raw_code() == -libc::ENODEV) =>
            {
                info!(bridge = %common.bridge, "bridge not found during lookup, creating it");
                let new_bridge = LinkBridge::new(&common.bridge).up().build();
                handle
                    .link()
                    .add(new_bridge)
                    .execute()
                    .await
                    .wrap_err_with(|| format!("failed to create bridge {}", common.bridge))?;

                handle
                    .link()
                    .get()
                    .match_name(common.bridge.clone())
                    .execute()
                    .next()
                    .await
                    .ok_or_else(|| eyre!("bridge {} was not found after creation", common.bridge))?
                    .wrap_err_with(|| {
                        format!("failed to query bridge {} after creation", common.bridge)
                    })?
            }
            Some(Err(err)) => {
                return Err(err)
                    .wrap_err_with(|| format!("failed to query bridge {}", common.bridge));
            }
            None => {
                info!(bridge = %common.bridge, "creating new bridge");
                let new_bridge = LinkBridge::new(&common.bridge).up().build();
                handle
                    .link()
                    .add(new_bridge)
                    .execute()
                    .await
                    .wrap_err_with(|| format!("failed to create bridge {}", common.bridge))?;

                handle
                    .link()
                    .get()
                    .match_name(common.bridge.clone())
                    .execute()
                    .next()
                    .await
                    .ok_or_else(|| eyre!("bridge {} was not found after creation", common.bridge))?
                    .wrap_err_with(|| {
                        format!("failed to query bridge {} after creation", common.bridge)
                    })?
            }
        };

        let dhcp_actor = if let Some(dhcp_config) = &args.dhcp_config {
            Some(
                DhcpActor::spawn_link(&actor_ref, (dhcp_config.clone(), common.bridge.clone()))
                    .await,
            )
        } else {
            None
        };

        let actor = Self {
            config: args,
            common,
            dhcp_actor,
            netlink_thread,
            netlink_handle: handle,
        };

        let common_bridge = actor.common.bridge.clone();
        let common_subnet = actor.common.subnet.clone();

        info!(
            bridge = %common_bridge,
            subnet = %common_subnet,
            upstream_iface = ?actor.common.upstream_iface,
            "network actor startup configuration resolved"
        );

        match actor.config.network_mode.clone() {
            NetworkMode::HostonlyNat {
                bridge: _,
                subnet,
                gateway,
                upstream_iface: _,
            } => {
                info!(
                    bridge = %actor.common.bridge,
                    gateway = %gateway,
                    prefix_len = subnet.prefix_len(),
                    "ensuring host-only NAT bridge gateway address"
                );

                ensure_address(
                    &actor.netlink_handle,
                    bridge.header.index,
                    &actor.common.bridge,
                    gateway,
                    subnet.prefix_len(),
                )
                .await
                .wrap_err_with(|| {
                    format!(
                        "failed to ensure gateway {}/{} exists on bridge {}",
                        gateway,
                        subnet.prefix_len(),
                        actor.common.bridge
                    )
                })?;

                let upstream_iface =
                    actor.common.upstream_iface.as_deref().ok_or_else(|| {
                        eyre!("host-only NAT mode requires an upstream interface")
                    })?;

                info!(
                    bridge = %common_bridge,
                    upstream_iface = upstream_iface,
                    "host-only NAT uses routed upstream interface rather than bridge master"
                );

                Self::ensure_nat_rules(&common_bridge, &common_subnet, upstream_iface)
                    .wrap_err_with(|| {
                        format!(
                            "failed to ensure nftables NAT rules for bridge {} and upstream {}",
                            common_bridge, upstream_iface
                        )
                    })?;
            }
            NetworkMode::Bridged {
                bridge: _,
                subnet,
                gateway,
            } => {
                info!(
                    bridge = %actor.common.bridge,
                    gateway = %gateway,
                    prefix_len = subnet.prefix_len(),
                    "ensuring bridged mode host address on bridge"
                );

                ensure_address(
                    &actor.netlink_handle,
                    bridge.header.index,
                    &actor.common.bridge,
                    gateway,
                    subnet.prefix_len(),
                )
                .await
                .wrap_err_with(|| {
                    format!(
                        "failed to ensure gateway {}/{} exists on bridge {}",
                        gateway,
                        subnet.prefix_len(),
                        actor.common.bridge
                    )
                })?;
            }
        }

        Ok(actor)
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> std::result::Result<(), Self::Error> {
        match reason {
            ActorStopReason::Normal => {
                info!(bridge = %self.common.bridge, "stopping network agent");
            }
            ActorStopReason::Killed => {
                info!(bridge = %self.common.bridge, "network agent killed");
            }
            ActorStopReason::Panicked(err) => {
                info!(bridge = %self.common.bridge, ?err, "network agent panicked");
            }
            _ => {
                info!(bridge = %self.common.bridge, "network agent stopping");
            }
        }

        if let Some(dhcp_actor) = self.dhcp_actor.take() {
            dhcp_actor.stop_gracefully().await?;
        }

        self.netlink_thread.abort();
        let _ = (&mut self.netlink_thread).await;

        Ok(())
    }
}

impl Message<AttachTap> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: AttachTap,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let bridge = self
            .lookup_link_by_name(&self.common.bridge)
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to resolve bridge {} before attaching tap {}",
                    self.common.bridge, msg.tap_name
                )
            })?;

        let tap = self
            .lookup_link_by_name(&msg.tap_name)
            .await
            .wrap_err_with(|| {
                format!("failed to resolve tap {} for vm {}", msg.tap_name, msg.vmid)
            })?;

        info!(
            vmid = %msg.vmid,
            tap = %msg.tap_name,
            bridge = %self.common.bridge,
            "attaching tap to bridge"
        );

        self.netlink_handle
            .link()
            .set(
                LinkUnspec::new_with_index(tap.header.index)
                    .controller(bridge.header.index)
                    .build(),
            )
            .execute()
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to attach tap {} to bridge {}",
                    msg.tap_name, self.common.bridge
                )
            })?;

        self.netlink_handle
            .link()
            .set(LinkUnspec::new_with_index(tap.header.index).up().build())
            .execute()
            .await
            .wrap_err_with(|| format!("failed to bring tap {} up", msg.tap_name))?;

        info!(
            vmid = %msg.vmid,
            tap = %msg.tap_name,
            bridge = %self.common.bridge,
            "tap attached to bridge successfully"
        );

        Ok(())
    }
}

impl Message<DetachTap> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: DetachTap,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let tap = self
            .lookup_link_by_name(&msg.tap_name)
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to resolve tap {} for detach on vm {}",
                    msg.tap_name, msg.vmid
                )
            })?;

        info!(
            vmid = %msg.vmid,
            tap = %msg.tap_name,
            bridge = %self.common.bridge,
            "detaching tap from bridge"
        );

        self.netlink_handle
            .link()
            .set(
                LinkUnspec::new_with_index(tap.header.index)
                    .nocontroller()
                    .build(),
            )
            .execute()
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to detach tap {} from bridge {}",
                    msg.tap_name, self.common.bridge
                )
            })?;

        info!(
            vmid = %msg.vmid,
            tap = %msg.tap_name,
            bridge = %self.common.bridge,
            "tap detached from bridge successfully"
        );

        Ok(())
    }
}
