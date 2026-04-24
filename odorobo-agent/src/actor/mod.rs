use crate::{networking::actor::NetworkAgentActor, state::provisioning::actor::VMActor};
use ahash::AHashMap;
use bytesize::ByteSize;
use ipnet::Ipv4Net;
use kameo::prelude::*;
use odorobo_shared::{
    actor_names::VM, messages::{Ping, Pong, agent::{AgentStatus, GetAgentStatus}, debug::PanicAgent, vm::*}, utils::vm_actor_id
};
use serde::{Deserialize, Serialize};
use stable_eyre::{Report, Result};
use std::ops::ControlFlow;
use std::{fs, net::Ipv4Addr};
use sysinfo::System;
use tracing::{error, info, trace, warn};
use ulid::Ulid;

use kameo::error::PanicError;

#[derive(RemoteActor)]
pub struct AgentActor {
    pub vcpus: u32,
    pub memory: ByteSize,
    pub config: Config,
    pub vms: AHashMap<Ulid, ActorRef<VMActor>>,
    // pub network_actor: ActorRef<NetworkAgentActor>,
}

/// Gets the system hostname
pub fn hostname() -> String {
    System::host_name().unwrap_or("odorobo".into())
}

/// This was requested by katherine. Do not change without asking her.
pub fn default_reserved_vcpus() -> u32 {
    2
}

fn default_datacenter() -> String {
    warn!("No datacenter specified, defaulting to Dev");

    "Dev".into()
}

fn default_region() -> String {
    warn!("No region specified, defaulting to Local");
    "Local".into()
}

fn default_bridge_name() -> String {
    "vmbr0".into()
}

fn default_subnet() -> Ipv4Net {
    "10.0.0.0/24".parse().unwrap()
}

fn default_gateway() -> Ipv4Addr {
    "10.0.0.1".parse().unwrap()
}
/// Infers the default upstream interface from the system's default route
fn default_upstream_iface() -> String {
    // ip route
    let out = std::process::Command::new("ip")
        .arg("route")
        .output()
        .unwrap();
    let output = String::from_utf8(out.stdout).unwrap();

    let default_route = output.lines().find(|l| l.starts_with("default")).unwrap();
    let iface = default_route.split_whitespace().nth(4).unwrap();
    info!("inferring default upstream interface: {}", iface);
    iface.into()
}

/// DHCP server config
///
/// config options for dnsmasq
///
/// this configures what options
// --no-daemon
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DhcpConfig {
    pub range: (Ipv4Addr, Ipv4Addr),
    pub subnet: Ipv4Net,
    /// lease time for DHCP clients
    ///
    /// example: 12h, 6h, 30m
    pub lease_time: String,
}

// TODO: move config into a separate module
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct NetworkConfig {
    pub dhcp_config: Option<DhcpConfig>,
    pub network_mode: NetworkMode,
}

/// L3 routing configuration for guests
#[derive(Serialize, Deserialize, Clone)]
pub enum NetworkMode {
    /// Private guest bridge with host-side gateway and outbound NAT.
    HostonlyNat {
        #[serde(default = "default_bridge_name")]
        bridge: String,
        #[serde(default = "default_subnet")]
        subnet: Ipv4Net,
        #[serde(default = "default_gateway")]
        gateway: Ipv4Addr,
        #[serde(default = "default_upstream_iface")]
        upstream_iface: String,
    },
    /// Flat bridge mode for operator-managed uplinks.
    ///
    /// The agent should only ensure that the bridge exists, is up, and has the
    /// configured host address on it. It should not automatically enslave a
    /// physical uplink into the bridge. Operators are expected to attach the
    /// upstream interface themselves and handle any host networking migration
    /// required for their environment.
    ///
    /// Per-VM TAP devices can still be attached to this bridge in the same way
    /// as NAT mode.
    Bridged {
        bridge: String,
        subnet: Ipv4Net,
        gateway: Ipv4Addr,
    },
}

impl Default for NetworkMode {
    fn default() -> Self {
        Self::HostonlyNat {
            bridge: default_bridge_name(),
            subnet: default_subnet(),
            gateway: default_gateway(),
            upstream_iface: default_upstream_iface(),
        }
    }
}

// The infra team wants a config file on the box where they can set info specific for the box its on.
// TODO: Double check with infra team (katherine) if they want any other config on the box.
#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    /// The hostname of the agent. Defaults to the system hostname
    /// if not specified in the config file.
    #[serde(default = "hostname")]
    pub hostname: String,
    /// The datacenter the agent is running in.
    #[serde(default = "default_datacenter")]
    pub datacenter: String,
    /// The region the agent is running in.
    #[serde(default = "default_region")]
    pub region: String,
    /// The number of VCPUs reserved for the agent. Defaults to 2.
    #[serde(default = "default_reserved_vcpus")]
    pub reserved_vcpus: u32,
    /// this is just arbitrary data that will be shown but does no config
    /// Arbitrary labels that can be used
    #[serde(default)]
    pub labels: AHashMap<String, String>,
    /// Arbitrary annotations that can be used
    #[serde(default)]
    pub annotations: AHashMap<String, String>,
    #[serde(default)]
    pub network: NetworkConfig,
}

impl AgentActor {
    async fn lookup_vm_actor(vmid: Ulid) -> Option<ActorRef<VMActor>> {
        ActorRef::<VMActor>::lookup(format!("vm:{}", vmid))
            .await
            .ok()
            .flatten()
    }
}

impl Actor for AgentActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        // TODO: ask infra team where they want this on the box
        let file = fs::File::open("config.json").expect("file should open read only");
        let config: Config = serde_json::from_reader(file).expect("file should be proper JSON");

        // spawn networking actor
        let network_actor: ActorRef<NetworkAgentActor> =
            NetworkAgentActor::spawn_link(&actor_ref, config.network.clone()).await;
        network_actor.register("network_actor").await?;

        let sys = System::new_all();

        Ok(AgentActor {
            vcpus: sys.cpus().len() as u32,
            memory: ByteSize::b(sys.total_memory()),
            config,
            vms: AHashMap::new(),
        })
    }

    // async fn on_panic(state: Self::Args, weak_actor_ref: WeakActorRef<Self>, _panic: &PanicError) {
    //     panic!("Agent panicked: {:?}", _panic);
    // }
    //
    async fn on_panic(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        err: PanicError,
    ) -> Result<std::ops::ControlFlow<ActorStopReason>> {
        error!("Agent panicked: {:?}", err);

        // todo: if we panic, we should completely regen the self struct from scratch. The assumption should be that memory corruption could have possibly happened becauew

        Ok(ControlFlow::Continue(()))
    }

    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>> {
        warn!("Linked actor {id:?} died with reason {reason:?}");

        self.vms.retain(|_, actor_ref| actor_ref.id() != id);

        Ok(ControlFlow::Continue(()))
    }
}

#[remote_message]
impl Message<CreateVM> for AgentActor {
    type Reply = CreateVMReply;

    async fn handle(&mut self, msg: CreateVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let vmid = msg.vmid;
        // spawn AND link at the same time
        let actor_ref =
            VMActor::spawn_link(ctx.actor_ref(), (vmid, Some(msg.config.clone()))).await;

        let _ = actor_ref.register(vm_actor_id(vmid)).await;
        let _ = actor_ref.register(VM).await;
        self.vms.insert(vmid, actor_ref.clone());

        info!(?vmid, "VM Spawned successfully");
        CreateVMReply {
            config: Some(msg.config),
        }
    }
}

#[remote_message]
impl Message<MigrateVMReceive> for AgentActor {
    type Reply = MigrateVMReceiveReply;

    async fn handle(
        &mut self,
        msg: MigrateVMReceive,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vmid = msg.vmid;
        let actor_ref = VMActor::spawn_link(ctx.actor_ref(), (vmid, None)).await;

        let _ = actor_ref.register(vm_actor_id(vmid)).await;
        let _ = actor_ref.register(VM).await;
        self.vms.insert(vmid, actor_ref.clone());

        // now ask the VM actor to handle the migration receive
        actor_ref
            .ask(msg)
            .await
            .expect("failed to start migration receiver on destination VM actor")
    }
}

#[remote_message]
impl Message<DeleteVM> for AgentActor {
    type Reply = DeleteVMReply;

    async fn handle(
        &mut self,
        msg: DeleteVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match self.vms.remove(&msg.vmid) {
            Some(actor_ref) => {
                let res = actor_ref.tell(msg.clone()).await;
                if let Err(err) = res {
                    // probably a bad way to do this
                    warn!(vm_id = %msg.vmid, ?err, "failed to stop VM actor gracefully, killing");
                    actor_ref.kill();
                }
            }
            None => {
                warn!(vm_id = %msg.vmid, "VM actor not found for delete");
            }
        }

        DeleteVMReply
    }
}

#[remote_message]
impl Message<ShutdownVM> for AgentActor {
    type Reply = Result<ShutdownVMReply, String>;

    async fn handle(
        &mut self,
        msg: ShutdownVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match Self::lookup_vm_actor(msg.vmid).await {
            Some(actor_ref) => {
                trace!(?msg, "Telling VM to shut down");
                let res = actor_ref.tell(msg.clone()).await;
                if let Err(err) = res {
                    warn!(vm_id = %msg.vmid, ?err, "failed to shutdown VM actor");
                }
            }
            None => {
                warn!(vm_id = %msg.vmid, "VM actor not found for shutdown");
                return Err("VM actor not found for shutdown".to_string());
            }
        }

        Ok(ShutdownVMReply)
    }
}
// forward GetVMInfo to VM actor
#[remote_message]
impl Message<GetVMInfo> for AgentActor {
    type Reply = ForwardedReply<GetVMInfo, GetVMInfoReply>;

    async fn handle(
        &mut self,
        msg: GetVMInfo,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // todo: caleb, i think this code can be cleaned up most likely, but im not sure what the best way to write it is unfortunately.
        match msg.vmid {
            Some(vmid) => {
                match Self::lookup_vm_actor(vmid).await {
                    Some(actor_ref) => ctx.forward(&actor_ref, msg).await,
                    None => {
                        warn!(vm_id = %vmid, "VM actor not found for info lookup");
                        ForwardedReply::from_ok(GetVMInfoReply { vmid, config: None })
                    }
                }
            },
            None => {
                warn!("No vmid provided for Agent Actor GetVMInfo forwarding");
                ForwardedReply::from_ok(GetVMInfoReply { vmid: Ulid::nil(), config: None })
            }
        }
    }
}

#[remote_message]
impl Message<AgentListVMs> for AgentActor {
    type Reply = AgentListVMsReply;

    async fn handle(
        &mut self,
        _msg: AgentListVMs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // look up with cache
        let vms = self.vms.keys().copied().collect();
        // let vms_actors: Vec<_> = RemoteActorRef::<VMActor>::lookup_all("vm").collect().await;

        // let mut vms = Vec::new();
        // for actor in vms_actors.into_iter().flatten() {
        //     trace!(?actor, "looking up VM info");
        //     if let Ok(reply) = actor.ask(&GetVMInfo).await {
        //         vms.push(reply.vmid);
        //     }
        // }

        AgentListVMsReply { vms }
    }
}

#[remote_message]
impl Message<Ping> for AgentActor {
    type Reply = Pong;

    async fn handle(&mut self, _msg: Ping, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        Pong
    }
}

#[remote_message]
impl Message<PanicAgent> for AgentActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: PanicAgent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        tracing::info!("panicking");
        panic!();
    }
}

#[remote_message]
impl Message<GetAgentStatus> for AgentActor {
    type Reply = AgentStatus;

    async fn handle(
        &mut self,
        _msg: GetAgentStatus,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {

        AgentStatus {
            hostname: self.config.hostname.clone(),
            vcpus: self.vcpus,
            ram: self.memory,
            vms: vec![Ulid::new()], // todo
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_config_serialize() {
        let config = Config {
            network: NetworkConfig {
                dhcp_config: Some(DhcpConfig {
                    range: (Ipv4Addr::new(10, 10, 1, 100), Ipv4Addr::new(10, 10, 1, 200)),
                    subnet: Ipv4Net::new(Ipv4Addr::new(10, 10, 1, 0), 24).unwrap(),
                    lease_time: "12h".to_string(),
                }),
                network_mode: NetworkMode::HostonlyNat {
                    bridge: "vmbr0".to_string(),
                    gateway: Ipv4Addr::new(10, 10, 100, 1),
                    subnet: Ipv4Net::new(Ipv4Addr::new(10, 10, 100, 0), 24).unwrap(),
                    upstream_iface: default_upstream_iface(),
                },
            },
            ..Default::default()
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        // assert_eq!(json, )
        println!("{}", json);
    }
}
