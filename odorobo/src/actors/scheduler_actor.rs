use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use tracing::trace;
use ulid::Ulid;
use crate::actors::agent_actor::AgentActor;
use crate::ch_driver::actor::VMActor;
use crate::utils::actor_names::VM;
use crate::messages::vm::*;
use crate::messages::agent::*;
use crate::messages::{Ping, Pong};
use crate::utils::actor_names::AGENT;
use crate::utils::actor_names::vm_actor_id;
use stable_eyre::eyre::OptionExt;
use stable_eyre::{Report, Result, eyre::eyre};
use tracing::info_span;
use tracing::{info, warn};
use dashmap::DashMap;
use tokio::task::JoinHandle;


#[derive(Debug, Clone)]
pub struct CachedAgentActor {
    pub actor_ref: RemoteActorRef<AgentActor>,
    pub metadata: AgentStatus,
}

#[derive(Debug, Clone)]
pub struct CachedVMActor {
    pub actor_ref: RemoteActorRef<VMActor>,
    pub metadata: GetVMInfoReply,
}



#[derive(RemoteActor)]
pub struct SchedulerActor {
    pub agent_data_cache: Arc<DashMap<ActorId, CachedAgentActor>>,
    pub agent_keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,

    // todo: we might need a better way to store this.
    //  we are likely going to want to store vms even if we don't know their actorid (ex: actor hasn't been started or is shutdown)
    //  but we also might want ot be able to store them without a ulid, possibly
    //  so we might need a vec of vms and then to just store maps/indexes of actorid and ulid to vector index
    //  and then like a freelist or something.
    //  i dont really love that option either though cause it feels overkill.
    //  maybe we sure just be using a proper database entirely?
    //  idk. will figure it out later.
    pub vm_actorid_ulid_map: Arc<DashMap<ActorId, Ulid>>,
    pub vm_data_cache: Arc<DashMap<Ulid, CachedVMActor>>,
    pub vm_keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,

    pub cache_actor_finder: Option<JoinHandle<()>>,
}

// todo: this might need to be a runtime thing but this makes it easy to write for now and could easily be switched out later.
static VCPU_OVERPROVISIONMENT_NUMERATOR: u32 = 2;
static VCPU_OVERPROVISIONMENT_DENOMINATOR: u32 = 1;


impl SchedulerActor {
    async fn lookup_agent_by_actor_id(
        &mut self,
        actor_id: &ActorId,
    ) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_data_cache.get(actor_id).map(|data| data.actor_ref.clone())
    }

    async fn lookup_agent_by_hostname(
        &mut self,
        hostname: &str,
    ) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_data_cache.iter().find(|data| data.metadata.hostname == hostname).map(|data| data.actor_ref.clone())
    }


    // someone should likely give caleb a firm talking to about code duplication due to this section, but things are just different enough that trying to make them one function requires usage of a lot of generics which feels even worse. so i dont know what to do. cappy please fix. i hate this.
    async fn vm_actor_finder(
        parent_actor_ref: RemoteActorRef<SchedulerActor>,
        vm_actorid_ulid_map: Arc<DashMap<ActorId, Ulid>>,
        data_cache: Arc<DashMap<Ulid, CachedVMActor>>,
        keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>
    ) -> Result<(), Report> {

        while let Some(vm_actor) = RemoteActorRef::<VMActor>::lookup_all(VM).try_next().await? {
            if !keepalive_tasks.contains_key(&vm_actor.id()) {
                trace!(?vm_actor, "starting vm_updater_task");

                parent_actor_ref.link_remote(&vm_actor).await?;

                let vm_actor_id = vm_actor.id();

                let vm_actorid_ulid_map_clone = Arc::clone(&vm_actorid_ulid_map);
                let data_cache_clone = Arc::clone(&data_cache);
                let updater_task = tokio::spawn(async move {
                    Self::vm_updater_task(
                        vm_actor,
                        vm_actorid_ulid_map_clone,
                        data_cache_clone
                    ).await;
                });

                keepalive_tasks.insert(
                    vm_actor_id,
                    updater_task
                );
            }
        }

        Ok(())

    }

    async fn vm_updater_task(
        actor_ref: RemoteActorRef<VMActor>,
        vm_actorid_ulid_map: Arc<DashMap<ActorId, Ulid>>,
        data_cache: Arc<DashMap<Ulid, CachedVMActor>>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut fails = 0;
        loop {
            if let Ok(metadata) = actor_ref.ask(&GetVMInfo {vmid: None}).await {
                let vmid = metadata.vmid;

                vm_actorid_ulid_map.insert(actor_ref.id(), vmid); // should we be doing this on every loop? idk. but we at least need to do it on the first iteration given we don't know the mapping before that

                data_cache.insert(
                    vmid,
                    CachedVMActor {
                        actor_ref: actor_ref.clone(),
                        metadata: metadata
                    }
                );

                fails = 0;
            } else {
                fails += 1;
            }

            if fails > 5 {
                // todo: possibly better error handling
                warn!(?actor_ref, "can no longer reach agent actor.")
            }

            interval.tick().await;
        }
    }

    async fn agent_actor_finder(
        parent_actor_ref: RemoteActorRef<SchedulerActor>,
        data_cache: Arc<DashMap<ActorId, CachedAgentActor>>,
        keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,
    ) -> Result<(), Report> {
        info!("running agent_actor_finder");
        while let Some(agent_actor) = RemoteActorRef::<AgentActor>::lookup_all(AGENT).try_next().await? {
            if !keepalive_tasks.contains_key(&agent_actor.id()) {
                trace!(?agent_actor, "starting agent_updater_task");

                parent_actor_ref.link_remote(&agent_actor).await?;

                let agent_actor_id = agent_actor.id();

                let data_cache_clone = Arc::clone(&data_cache);
                let updater_task = tokio::spawn(async move {
                    Self::agent_updater_task(
                        agent_actor,
                        data_cache_clone
                    ).await;
                });

                keepalive_tasks.insert(
                    agent_actor_id,
                    updater_task
                );
            }
        }

        Ok(())
    }

    async fn agent_updater_task(
        actor_ref: RemoteActorRef<AgentActor>,
        data_cache: Arc<DashMap<ActorId, CachedAgentActor>>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut fails = 0;
        loop {
            if let Ok(metadata) = actor_ref.ask(&GetAgentStatus).await {
                data_cache.insert(
                    actor_ref.id(),
                    CachedAgentActor {
                        actor_ref: actor_ref.clone(),
                        metadata: metadata
                    }
                );

                fails = 0;
            } else {
                fails += 1;
            }

            if fails > 5 {
                // todo: possibly better error handling
                warn!(?actor_ref, "can no longer reach agent actor.")
            }

            interval.tick().await;
        }
    }

    fn start_actor_finder(&mut self, actor_ref: RemoteActorRef<Self>) {
        let agent_data_cache_arc_clone = Arc::clone(&self.agent_data_cache);
        let agent_keepalive_tasks_arc_clone = Arc::clone(&self.agent_keepalive_tasks);

        let vm_actorid_ulid_map_arc_clone = Arc::clone(&self.vm_actorid_ulid_map);
        let vm_data_cache_arc_clone = Arc::clone(&self.vm_data_cache);
        let vm_keepalive_tasks_arc_clone = Arc::clone(&self.vm_keepalive_tasks);

        self.cache_actor_finder = Some(
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                loop {
                    let vm_join_handle = Self::vm_actor_finder(
                        actor_ref.clone(),
                        Arc::clone(&vm_actorid_ulid_map_arc_clone),
                        Arc::clone(&vm_data_cache_arc_clone),
                        Arc::clone(&vm_keepalive_tasks_arc_clone),
                    );

                    let agent_join_handle = Self::agent_actor_finder(
                        actor_ref.clone(),
                        Arc::clone(&agent_data_cache_arc_clone),
                        Arc::clone(&agent_keepalive_tasks_arc_clone),
                    );

                    // intentionally ignoring results because we want to keep finding actors even if an attempt fails
                    let _ = tokio::join!(vm_join_handle, agent_join_handle);

                    interval.tick().await;
                }
            })
        );

    }


    /// current scheduling algo info:
    /// this is vaguely based on https://kubernetes.io/docs/concepts/scheduling-eviction/assign-pod-node/
    /// when a vm is attempted to be scheduled, we loop through every agent and score it based on some rules
    /// there are hard rules that will simply throw out an agent entirely.
    /// otherwise, we take whatever the best agent we can find is.
    async fn schedule_agent(
        &mut self,
        msg: &CreateVM
    ) -> Result<RemoteActorRef<AgentActor>, Report> {
        let mut best_agent = None;
        let mut best_agent_score = 0.0f32;

        // todo: this arguably could be done as map-reduce. is that better?
        let span = info_span!("schedule_agent");
        span.in_scope(|| {
            for agent in self.agent_data_cache.iter() {
                let mut agent_score = 0.0f32;

                let agent_max_vcpus = agent.metadata.vcpus * VCPU_OVERPROVISIONMENT_NUMERATOR / VCPU_OVERPROVISIONMENT_DENOMINATOR;
                // todo: do we care about VMData.max_vcpus?
                let agent_used_vcpus = agent.metadata.used_vcpus + msg.config.data.vcpus;

                if agent_used_vcpus >= agent_max_vcpus {
                    continue;
                }

                agent_score += (agent_max_vcpus - agent_used_vcpus) as f32 / agent_max_vcpus as f32;


                // todo: add ram overprovisionment.     not adding this to scheduler until it works on the hypervisor side.
                let agent_max_ram = agent.metadata.ram;
                let agent_used_ram = agent.metadata.used_ram + msg.config.data.memory;

                if agent_used_ram >= agent_max_ram {
                    continue;
                }

                agent_score += (agent_max_ram.as_u64() - agent.metadata.used_ram.as_u64()) as f32 / agent_max_ram.as_u64() as f32;


                // todo: https://kubernetes.io/docs/concepts/scheduling-eviction/assign-pod-node/



                // todo (future): possibly keep a percent of agents completely empty, to be able to be converted to dedis automatically.
                // they would have their agent score set to 1, so they can be scheduled to if there is no other avaliable agents.
                // rough pseudo code to implement this:
                // if agent.metadata.vms.len() == 0 && hash(agent.config.hostname) % total_chance < threshold {
                //     agent_score = 1;
                // }



                info!(agent=?agent.value(), score=agent_score);

                if agent_score > best_agent_score {
                    best_agent = Some(agent.actor_ref.clone());
                    best_agent_score = agent_score;
                }
            }
        });

        best_agent.ok_or_eyre("No valid agents found.")
    }
}



impl Actor for SchedulerActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let peer_id = *actor_ref.id().peer_id().unwrap();

        info!(?peer_id, "Scheduler Actor started!");

        let mut scheduler_actor = SchedulerActor {
            agent_data_cache: Arc::new(DashMap::new()),
            agent_keepalive_tasks: Arc::new(DashMap::new()),
            vm_actorid_ulid_map: Arc::new(DashMap::new()),
            vm_data_cache: Arc::new(DashMap::new()),
            vm_keepalive_tasks: Arc::new(DashMap::new()),
            cache_actor_finder: None,
        };

        scheduler_actor.start_actor_finder(actor_ref.into_remote_ref().await);

        Ok(scheduler_actor)
    }


    async fn on_link_died(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        actor_id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        warn!(?actor_id, ?reason, "Linked actor died");

        // check that scheduler actor is still alive.
        let Some(_) = actor_ref.upgrade() else {
            return Ok(ControlFlow::Break(ActorStopReason::Killed));
        };


        if let Some((_, keepalive_task)) = self.agent_keepalive_tasks.remove(&actor_id) {
            trace!(?actor_id, "Aborting agent keepalive task");
            keepalive_task.abort();
        };


        self.agent_data_cache.remove(&actor_id);

        // todo: attempt vm migration or restart or whatever on agent death.

        if let Some((_, keepalive_task)) = self.vm_keepalive_tasks.remove(&actor_id) {
            trace!(?actor_id, "Aborting vm keepalive task");
            keepalive_task.abort();
        };

        if let Some((_, vmid)) = self.vm_actorid_ulid_map.remove(&actor_id) {
            // todo: we likely should keep a copy of the VirtualMachine manifest in the cache.
            //  instead of removing the vm entirely, we should just modify the status to shutdown or crashed or something.
            trace!(?actor_id, ?vmid, "Removing vm from vm_data_cache");
            self.vm_data_cache.remove(&vmid);
        }


        Ok(ControlFlow::Continue(()))
    }
}




impl Message<CreateVM> for SchedulerActor {
    type Reply = Result<CreateVMReply, Report>;

    async fn handle(&mut self, msg: CreateVM, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        loop {
            let target_agent = self.schedule_agent(&msg).await?;

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
        tracing::trace!(?vm, "DeleteVM");
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
        tracing::trace!(?vm, "ShutdownVM");
        if let Some(vm) = vm {
            vm.tell(&msg).send()?;
            Ok(ShutdownVMReply)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

/// this only gets data from the cache from agents
/// we may need a different message that actually forcibly runs/updates everything.
/// and/or messages that get data directly from the VMActors.
impl Message<AgentListVMs> for SchedulerActor {
    type Reply = Result<AgentListVMsReply, Report>;

    async fn handle(
        &mut self,
        _msg: AgentListVMs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut vms = Vec::new();

        for agent in self.agent_data_cache.iter() {
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
