use std::ops::ControlFlow;

use async_trait::async_trait;
use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use odorobo_agent::actor::AgentActor;
use odorobo_agent::state::provisioning::actor::VMActor;
use odorobo_shared::actor_cache::ActorCache;
use odorobo_shared::actor_cache::ActorCacheUpdater;
use odorobo_shared::messages::vm::*;
use odorobo_shared::messages::agent::*;
use odorobo_shared::messages::{Ping, Pong};
use odorobo_shared::utils::vm_actor_id;
use stable_eyre::{Report, Result, eyre::eyre};
use tracing::{info, warn};


#[derive(RemoteActor)]
pub struct SchedulerActor {
    pub agent_actor_cache: ActorCache<SchedulerActor, AgentActor, CachedAgentActor>

    //pub vm_actor_cache: Arc<Mutex<AHashMap<ActorId, CachedVMActor>>>,
    //pub vm_keepalive_task: Option<tokio::task::JoinHandle<()>>,
}

impl SchedulerActor {
    async fn lookup_by_actor_id(
        &mut self,
        actor_id: &ActorId,
    ) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_actor_cache.lock_data_cache().await.get(actor_id).map(|data| data.actor_ref.clone())
    }

    async fn lookup_by_hostname(
        &mut self,
        hostname: &str,
    ) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_actor_cache.lock_data_cache().await.values().find(|data| data.metadata.hostname == hostname).map(|data| data.actor_ref.clone())
    }

    async fn schedule_agent(
        &mut self,
        msg: &CreateVM
    ) -> RemoteActorRef<AgentActor> {

        let locked_agent_actor_cache = self.agent_actor_cache.lock_data_cache().await;

        let agents: Vec<&CachedAgentActor> = locked_agent_actor_cache.values().collect();

        warn!("randomly selecting agent");
        // random agent selection, so basically round robin
        // todo: improve this with something that like actually schedules agents properly
        let agent_index = rand::random_range(0..agents.len());

        agents[agent_index].actor_ref.clone()
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



#[derive(Copy, Clone)]
struct AgentActorCacheUpdater;

#[derive(Debug, Clone)]
pub struct CachedAgentActor {
    pub actor_ref: RemoteActorRef<AgentActor>,
    pub metadata: AgentStatus,
}

#[async_trait]
impl ActorCacheUpdater<AgentActor, CachedAgentActor> for AgentActorCacheUpdater {
    async fn get_actor_refs(&self) -> Result<Vec<RemoteActorRef<AgentActor>>> {
        let mut agent_actors_lookup = RemoteActorRef::<AgentActor>::lookup_all("agent");
        let mut actor_ref_vec = Vec::new();

        while let Some(agent_actor) = agent_actors_lookup.try_next().await? {
            actor_ref_vec.push(agent_actor);
        }

        Ok(actor_ref_vec)
    }

    async fn on_update(&self, actor_ref: &RemoteActorRef<AgentActor>, previous_value: Option<CachedAgentActor>) -> Result<CachedAgentActor, Report> {
        let output_actor_ref = match previous_value {
            Some(value) => value.actor_ref,
            _ => actor_ref.clone(),
        };

        Ok(CachedAgentActor {
            actor_ref: output_actor_ref,
            metadata: actor_ref.ask(&GetAgentStatus).await?
        })
    }
}



impl Actor for SchedulerActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let peer_id = *actor_ref.id().peer_id().unwrap();

        info!("Actor started! Scheduler peer id: {peer_id}");

        Ok(Self {
            agent_actor_cache: ActorCache::new(actor_ref, AgentActorCacheUpdater)?,
        })
    }


    async fn on_link_died(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        warn!("Linked actor {id:?} died with reason {reason:?}");

        let Some(_) = actor_ref.upgrade() else {
            return Ok(ControlFlow::Break(ActorStopReason::Killed));
        };

        self.agent_actor_cache.info().await;

        self.agent_actor_cache.on_link_died(id).await;

        self.agent_actor_cache.info().await;

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

/// this only gets data from the cache.
/// we may need a different message that actually forcibly runs/updates everything.
impl Message<AgentListVMs> for SchedulerActor {
    type Reply = Result<AgentListVMsReply, Report>;

    async fn handle(
        &mut self,
        _msg: AgentListVMs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut vms = Vec::new();

        for agent in self.agent_actor_cache.lock_data_cache().await.values() {
            vms.extend_from_slice(agent.metadata.vms.as_slice());
        }

        Ok(AgentListVMsReply { vms })
    }
}



impl Message<Ping> for SchedulerActor {
    type Reply = Pong;

    async fn handle(&mut self, _msg: Ping, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        Pong
    }
}
