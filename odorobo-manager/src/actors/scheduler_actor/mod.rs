pub mod actor_keepalive;

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use ahash::AHashMap;
use async_trait::async_trait;
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
use tokio::task::JoinSet;
use tracing::{info, warn, trace, debug};
use ulid::Ulid;

use crate::actors::scheduler_actor::actor_keepalive::ActorAgentKeepalive;
use crate::actors::scheduler_actor::actor_keepalive::CachedAgentActor;
use crate::actors::scheduler_actor::actor_keepalive::CachedVMActor;
use crate::actors::scheduler_actor::actor_keepalive::keepalive_agents;


// todo:
// I (caleb) do not like the way this is written.
// I worry the Arc<Mutex<T>> is going to result in contention and issues, when we have large numbers of VMs.
//
// The contention will be caused by the fact that we do a ping to check the status of each VM/agent once per second.
// If we are running 1000s of VMs in the future, one hashmap to store all of that is eventually going to have latency problems
// Especially since the hashmap is also used whenever a user wants to make a change or things change in the swarm.
//
// I am leaving it this way because I just wanted to get things working. We may need to change the way the data is stored in the future.
//
// Either way, I think we should write a generic struct that acts as a supervisor "task".
// It would automatically manage a map for a set of actors, so you just have to run the supervisor in the task.
// This would let us reuse it other places.
// It would also likely need a hook to be able to run generic code during the keepalive function so you can also cache metadata such as VM status
//
// The keepalive tasks map can likely be kept with a RwLock,
// since it is mostly just reads unless a VMActor is actually created/destroyed, which should be less common.
#[derive(RemoteActor)]
pub struct SchedulerActor {
    pub agent_actor_cache: Arc<Mutex<AHashMap<ActorId, CachedAgentActor>>>,
    pub agent_actor_keepalive_tasks: Arc<Mutex<AHashMap<ActorId, ActorAgentKeepalive>>>,
    pub agent_keepalive_task: Option<tokio::task::JoinHandle<()>>,

    //pub vm_actor_cache: Arc<Mutex<AHashMap<ActorId, CachedVMActor>>>,
    //pub vm_keepalive_task: Option<tokio::task::JoinHandle<()>>,
}

impl SchedulerActor {
    async fn lookup_by_actor_id(
        &mut self,
        actor_id: &ActorId,
    ) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_actor_cache.lock().await.get(actor_id).map(|data| data.actor_ref.clone())
    }

    async fn lookup_by_hostname(
        &mut self,
        hostname: &str,
    ) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_actor_cache.lock().await.values().find(|data| data.metadata.hostname == hostname).map(|data| data.actor_ref.clone())
    }

    async fn schedule_agent(
        &mut self,
        msg: &CreateVM
    ) -> RemoteActorRef<AgentActor> {

        let locked_agent_actor_cache = self.agent_actor_cache.lock().await;

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
        let agent_actors_keepalives = Arc::new(Mutex::new(AHashMap::new()));

        //let vm_actors = Arc::new(Mutex::new(AHashMap::new()));

        let actor_ref_clone = actor_ref.clone();
        let agent_actors_clone = Arc::clone(&agent_actors);
        let agent_actors_keepalives_clone = Arc::clone(&agent_actors_keepalives);

        let agent_keepalive_task = tokio::spawn(async move {
            keepalive_agents(actor_ref_clone, agent_actors_clone, agent_actors_keepalives_clone).await;
        });

        Ok(Self {
            agent_actor_cache: agent_actors,
            agent_actor_keepalive_tasks: agent_actors_keepalives,
            agent_keepalive_task: Some(agent_keepalive_task),


            //vm_actor_cache: vm_actors,
            //vm_keepalive_task: None
        })
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

        let printed_pre_keepalives = self.agent_actor_keepalive_tasks.lock().await;
        let printed_pre_cache = self.agent_actor_cache.lock().await;

        info!("agent actor cache data pre removal");
        info!("keepalives: {printed_pre_keepalives:?}");
        info!("actor_cache: {printed_pre_cache:?}");

        drop(printed_pre_cache);
        drop(printed_pre_keepalives);

        if let Some(mut agent_actor_keepalive) = self.agent_actor_keepalive_tasks.lock().await.remove(&id) {
            if let Some(task) = agent_actor_keepalive.keepalive_task.take() {
                trace!("Aborting keepalive task for agent {id:?}");
                task.abort();
            }
        };

        self.agent_actor_cache.lock().await.remove(&id);

        let printed_post_keepalives = self.agent_actor_keepalive_tasks.lock().await;
        let printed_post_cache = self.agent_actor_cache.lock().await;

        info!("agent actor cache data post removal");
        info!("keepalives: {printed_post_keepalives:?}");
        info!("actor_cache: {printed_post_cache:?}");

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
        msg: AgentListVMs,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let actor_ref = ctx.actor_ref();

        //let agent_actor_refs: Vec<&CachedAgentActor> = self.agent_actor_cache.lock().await.values().collect();

        let mut vms = Vec::new();

        for agent in self.agent_actor_cache.lock().await.values() {
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
