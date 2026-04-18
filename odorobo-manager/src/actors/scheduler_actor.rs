use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use ahash::AHashMap;
use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use odorobo_agent::actor::AgentActor;
use odorobo_agent::state::provisioning::actor::VMActor;
use odorobo_shared::messages::vm::*;
use odorobo_shared::messages::agent::*;
use odorobo_shared::messages::{Ping, Pong};
use odorobo_shared::utils::vm_actor_id;
use stable_eyre::{Report, Result, eyre::eyre};
use tokio::sync::Mutex;
use tracing::{info, warn, trace, debug};
use ulid::Ulid;

// this arguably should be renamed or something, but I wasn't sure what data to use here.
pub struct AgentActorDataCache {
    pub actor_ref: RemoteActorRef<AgentActor>,
    pub keepalive_task: Option<tokio::task::JoinHandle<()>>,
    pub metadata: AgentStatus,

}

// this arguably should be renamed or something, but I wasn't sure what data to use here.
pub struct VMActorDataCache {
    pub vm_actor_ref: RemoteActorRef<VMActor>,
    // task that constantly looks up every agent and updates status for it
    pub keepalive_task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(RemoteActor)]
pub struct SchedulerActor {
    pub agent_actors: Arc<Mutex<AHashMap<ActorId, AgentActorDataCache>>>,
    pub vms: AHashMap<Ulid, VMActorDataCache>,
    pub keepalive_task: Option<tokio::task::JoinHandle<()>>,
}

impl SchedulerActor {
    // async fn lookup_agent(
    //     &self,
    //     actor_ref: &ActorRef<Self>,
    // ) -> Result<RemoteActorRef<AgentActor>, Report> {

    //         let agent_actor_option = RemoteActorRef::<AgentActor>::lookup("agent").await?;

    //         let Some(agent_actor) = agent_actor_option else {
    //             warn!("No agent actor currently registered, retrying lookup");
    //             tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    //             return self.lookup_agent(actor_ref).await;
    //         };

    //         let agent_actor_peer_id = *agent_actor.id().peer_id().unwrap();
    //         info!("Using agent actor peer id: {agent_actor_peer_id}");

    //         // remotely link actor, on link death it will be automatically unlinked
    //         info!("Linking agent actor: {agent_actor_peer_id}");
    //         actor_ref.link_remote(&agent_actor).await?;

    //         return Ok(agent_actor);

    // }

    async fn lookup_by_actor_id(
        &mut self,
        actor_id: &ActorId,
    ) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_actors.lock().await.get(actor_id).map(|data| data.actor_ref.clone())
    }

    async fn lookup_by_hostname(
        &mut self,
        hostname: &str,
    ) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_actors.lock().await.values().find(|data| data.metadata.hostname == hostname).map(|data| data.actor_ref.clone())
    }

    async fn schedule_agent(
        &mut self,
        msg: &CreateVM
    ) -> RemoteActorRef<AgentActor> {

        let agents: Vec<&AgentActorDataCache> = self.agent_actors.lock().await.values().collect();

        // warn!("randomly selecting agent");
        let agent_index = 0; // todo: im lazy. make this random or something i guess.
        // random::

        agents[agent_index].actor_ref
    }
    /*

    async fn update_vms(
        &mut self
    ) -> Result<(), Report> {
        let mut vm_actors = RemoteActorRef::<VMActor>::lookup_all("agent");
        let mut get_vm_info_set = JoinSet::new();

        while let Some(vm_actor) = vm_actors.try_next().await? {
            get_vm_info_set.spawn(async move {
                tracing::trace!("AgentListVMs: vm_actor={:?}", vm_actor);

                vm_actor.ask(&GetVMInfo { vmid: ulid::Ulid::nil() }).await
            })
        }
    }

    */

}
/// Periodically sends a keepalive request to all agent actors and updates their metadata.
async fn keepalive_agents(
    agent_actors: Arc<Mutex<AHashMap<ActorId, AgentActorDataCache>>>
) -> Result<(), Report> {
    loop {
        let mut agent_actors_lookup = RemoteActorRef::<AgentActor>::lookup_all("agent");

        while let Some(agent_actor) = agent_actors_lookup.try_next().await? {
            tracing::trace!("UpdateAgents: agent_actor={:?}", agent_actor);
            let metadata = agent_actor.ask(&GetAgentStatus).await?;

            let mut locked_agent_actors = agent_actors.lock().await;

            if !locked_agent_actors.contains_key(&agent_actor.id()) {
                let cloned_agent_actor = agent_actor.clone();
                let task = tokio::spawn(async move {
                    update_agent(cloned_agent_actor).await
                });


                locked_agent_actors.insert(
                    agent_actor.id(),
                    AgentActorDataCache {
                        actor_ref: agent_actor,
                        keepalive_task: Some(task),
                        metadata,
                    }
                );
            } else {
                locked_agent_actors.get_mut(&agent_actor.id()).unwrap().metadata = metadata;
            }
        }
    }
}

async fn update_agent(
    agent_actor_ref: RemoteActorRef<AgentActor>
) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Err(_) = agent_actor_ref.ask(&Ping).await {
            break;
        }
    }
}

impl Actor for SchedulerActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let peer_id = *actor_ref.id().peer_id().unwrap();

        info!("Actor started! Scheduler peer id: {peer_id}");

        let agent_actors = Arc::new(Mutex::new(AHashMap::new()));

        let mut finished_actor = Self {
            agent_actors: Arc::clone(&agent_actors),
            vms: AHashMap::new(),
            keepalive_task: None
        };

        let keepalive_task = tokio::spawn(async move {
            keepalive_agents(agent_actors);
        });

        finished_actor.keepalive_task = Some(keepalive_task);

        Ok(finished_actor)
    }


    async fn on_link_died(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        warn!("Linked actor {id:?} died with reason {reason:?}");

        let Some(actor_ref) = actor_ref.upgrade() else {
            return Ok(ControlFlow::Break(ActorStopReason::Killed));
        };

        let mut locked_agent_actors = self.agent_actors.lock().await;

        let Some(agent_actor_data) = locked_agent_actors.get_mut(&id) else {
            return Ok(ControlFlow::Break(ActorStopReason::Killed));
        };

        if let Some(task) = agent_actor_data.keepalive_task.take() {
            trace!("Aborting keepalive task for agent {id:?}");
            task.abort();
        }

        locked_agent_actors.remove(&id);

        Ok(ControlFlow::Continue(()))
    }
}




impl Message<CreateVM> for SchedulerActor {
    type Reply = Result<CreateVMReply, Report>;

    async fn handle(&mut self, msg: CreateVM, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        loop {
            let target_agent = self.schedule_agent(&msg).await;

            match target_agent.ask(&msg).await {
                Ok(reply) => {
                    return Ok(reply)
                },
                Err(err) => {
                    warn!(
                        "CreateVM forwarding failed, trying again: {err}"
                    );
                }
            }
        }
    }
}

impl Message<DeleteVM> for SchedulerActor {
    type Reply = Result<DeleteVMReply, Report>;

    async fn handle(
        &mut self,
        msg: DeleteVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?;
        tracing::trace!("DeleteVM: vm={:?}", vm);
        if let Some(vm) = vm {
            vm.tell(&msg).send()?;
            Ok(DeleteVMReply)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

impl Message<ShutdownVM> for SchedulerActor {
    type Reply = Result<ShutdownVMReply, Report>;

    async fn handle(
        &mut self,
        msg: ShutdownVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?;
        tracing::trace!("ShutdownVM: vm={:?}", vm);
        if let Some(vm) = vm {
            vm.tell(&msg).send()?;
            Ok(ShutdownVMReply)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

impl Message<AgentListVMs> for SchedulerActor {
    type Reply = Result<AgentListVMsReply, Report>;

    async fn handle(
        &mut self,
        msg: AgentListVMs,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // Direct VM discovery attempt kept for reference, but it does not work reliably.
        // let mut vm_actors = RemoteActorRef::<VMActor>::lookup_all("vm");
        // let mut vms = Vec::new();
        //
        // while let Some(vm_actor) = vm_actors.try_next().await? {
        //     tracing::trace!("AgentListVMs: vm_actor={:?}", vm_actor);
        //
        //     match vm_actor.ask(&GetVMInfo { vmid: ulid::Ulid::nil() }).await {
        //         Ok(reply) => vms.push(reply.vmid),
        //         Err(err) => warn!("failed to query VM actor info while listing VMs: {err}"),
        //     }
        // }
        //
        // Ok(AgentListVMsReply { vms })

        let actor_ref = ctx.actor_ref();

        let first_agent = self.ensure_agent(actor_ref).await?;
        match first_agent.ask(&msg).await {
            Ok(reply) => Ok(reply),
            Err(first_err) => {
                warn!(
                    "AgentListVMs forwarding failed, clearing cached agent and retrying lookup: {first_err}"
                );
                self.agent_actor = None;

                let retry_agent = self.ensure_agent(actor_ref).await?;
                retry_agent.ask(&msg).await.map_err(|retry_err| {
                    eyre!(
                        "failed to forward AgentListVMs to agent actor after reconnect; first error: {first_err}; retry error: {retry_err}"
                    )
                })
            }
        }
    }
}



impl Message<Ping> for SchedulerActor {
    type Reply = Pong;

    async fn handle(&mut self, _msg: Ping, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        Pong
    }
}
