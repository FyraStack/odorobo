use crate::state::provisioning::actor::VMActor;
use ahash::AHashMap;
use bytesize::ByteSize;
use kameo::error::ActorStopReason;
use kameo::prelude::*;
use odorobo_shared::messages::create_vm::*;
use odorobo_shared::messages::debug::PanicAgent;
use odorobo_shared::messages::{Ping, Pong};
use serde::{Deserialize, Serialize};
use stable_eyre::{Report, Result};
use std::fs;
use std::ops::ControlFlow;
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

// The infra team wants a config file on the box where they can set info specific for the box its on.
// TODO: Double check with infra team (katherine) if they want any other config on the box.
#[derive(Serialize, Deserialize)]
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
}

impl Actor for AgentActor {
    type Args = ();
    type Error = Report;

    async fn on_start(state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        // TODO: ask infra team where they want this on the box
        let file = fs::File::open("config.json").expect("file should open read only");
        let config: Config = serde_json::from_reader(file).expect("file should be proper JSON");

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
        actor_ref: WeakActorRef<Self>,
        err: PanicError,
    ) -> Result<std::ops::ControlFlow<ActorStopReason>> {
        error!("Agent panicked: {:?}", err);

        // todo: if we panic, we should completely regen the self struct from scratch. The assumption should be that memory corruption could have possibly happened becauew

        Ok(ControlFlow::Continue(()))
    }
}

#[remote_message]
impl Message<CreateVM> for AgentActor {
    type Reply = CreateVMReply;

    async fn handle(&mut self, msg: CreateVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let vmid = msg.vm_id;
        let actor_ref = VMActor::spawn((vmid, msg.config.clone()));

        let _ = actor_ref.register(vmid.to_string()).await;
        self.vms.insert(vmid, actor_ref.clone());

        trace!(?vmid, "spawned VM actor, linking to context");
        ctx.actor_ref().link(&actor_ref).await;

        info!(?vmid, "VM Spawned successfully");
        CreateVMReply {
            config: Some(msg.config),
        }
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
        match self.vms.remove(&msg.vm_id) {
            Some(actor_ref) => {
                let res = actor_ref.stop_gracefully().await;
                if let Err(err) = res {
                    // probably a bad way to do this
                    warn!(vm_id = %msg.vm_id, ?err, "failed to stop VM actor gracefully, killing");
                    actor_ref.kill();
                }
            }
            None => {
                warn!(vm_id = %msg.vm_id, "VM actor not found for delete");
            }
        }

        DeleteVMReply
    }
}

#[remote_message]
impl Message<ShutdownVM> for AgentActor {
    type Reply = ShutdownVMReply;

    async fn handle(
        &mut self,
        msg: ShutdownVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match self.vms.get(&msg.vm_id) {
            Some(actor_ref) => {
                let res = actor_ref.stop_gracefully().await;
                if let Err(err) = res {
                    // probably a bad way to do this
                    warn!(vm_id = %msg.vm_id, ?err, "failed to stop VM actor gracefully, killing");
                    actor_ref.kill();
                }
            }
            None => {
                warn!(vm_id = %msg.vm_id, "VM actor not found for shutdown");
            }
        }

        ShutdownVMReply
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
        let vms = self.vms.keys().copied().collect();

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
        msg: PanicAgent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        tracing::info!("panicking");
        panic!();
    }
}

#[cfg(test)]
mod tests {}
