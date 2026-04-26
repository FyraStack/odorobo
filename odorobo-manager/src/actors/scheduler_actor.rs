use std::ops::ControlFlow;

use async_trait::async_trait;
use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use odorobo_agent::actor::AgentActor;
use odorobo_agent::state::provisioning::actor::VMActor;
use odorobo_shared::actor_cache::ActorCache;
use odorobo_shared::actor_cache::ActorCacheUpdater;
use odorobo_shared::actor_names::VM;
use odorobo_shared::messages::vm::*;
use odorobo_shared::messages::agent::*;
use odorobo_shared::messages::{Ping, Pong};
use odorobo_shared::actor_names::AGENT;
use odorobo_shared::utils::vm_actor_id;
use stable_eyre::eyre::OptionExt;
use stable_eyre::{Report, Result, eyre::eyre};
use tracing::info_span;
use tracing::{info, warn};


#[derive(RemoteActor)]
pub struct SchedulerActor {
    pub agent_actor_cache: ActorCache<SchedulerActor, AgentActor, CachedAgentActor>,
    pub vm_actor_cache: ActorCache<SchedulerActor, VMActor, CachedVMActor>
}

// todo: this might need to be a runtime thing but this makes it easy to write for now and could easily be switched out later.
static VCPU_OVERPROVISIONMENT_NUMERATOR: u32 = 2;
static VCPU_OVERPROVISIONMENT_DENOMINATOR: u32 = 1;


impl SchedulerActor {
    async fn lookup_by_actor_id(
        &mut self,
        actor_id: &ActorId,
    ) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_actor_cache.data_cache.get(actor_id).map(|data| data.actor_ref.clone())
    }

    async fn lookup_by_hostname(
        &mut self,
        hostname: &str,
    ) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_actor_cache.data_cache.iter().find(|data| data.metadata.hostname == hostname).map(|data| data.actor_ref.clone())
    }

    /// current scheduling algo info:
    /// this is vaguely based on https://kubernetes.io/docs/concepts/scheduling-eviction/assign-pod-node/
    /// when a vm is attempted to be scheduled, we loop through every agent and score it based on some rules
    /// there are hard rules that will simply throw out an agent entirely.
    /// otherwise, we take whatever the best agent we can find is.
    ///
    /// additionally, because caleb is way too performance brained, he used integer math for the entire scoring algorithm just so we didnt have to convert to floats.
    async fn schedule_agent(
        &mut self,
        msg: &CreateVM
    ) -> Result<RemoteActorRef<AgentActor>, Report> {
        let mut best_agent = None;
        let mut best_agent_score = 0u32;

        // todo: this arguably could be done as map-reduce. is that better?
        let span = info_span!("schedule_agent");
            span.in_scope(|| {
            for agent in self.agent_actor_cache.data_cache.iter() {
                let mut agent_score = 0u32;

                let agent_max_vcpus = agent.metadata.vcpus * VCPU_OVERPROVISIONMENT_NUMERATOR / VCPU_OVERPROVISIONMENT_DENOMINATOR;



                if agent.metadata.used_vcpus >= agent_max_vcpus {
                    continue;
                }

                agent_score += (agent_max_vcpus - agent.metadata.used_vcpus) * 1024 / agent_max_vcpus;


                // todo: add ram overprovisionment.     not adding this to scheduler until it works on the hypervisor side.
                let agent_max_ram = agent.metadata.ram;

                if agent.metadata.used_ram >= agent_max_ram {
                    continue;
                }

                agent_score += ((agent_max_ram.as_u64() - agent.metadata.used_ram.as_u64()) * 1024 / agent_max_ram.as_u64()) as u32;


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
        let mut agent_actors_lookup = RemoteActorRef::<AgentActor>::lookup_all(AGENT);
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


// todo: this code is really bad, and we should not have effectively two copies of ths same thing.
#[derive(Copy, Clone)]
struct VMActorCacheUpdater;

#[derive(Debug, Clone)]
pub struct CachedVMActor {
    pub actor_ref: RemoteActorRef<VMActor>,
    pub metadata: GetVMInfoReply,
}

#[async_trait]
impl ActorCacheUpdater<VMActor, CachedVMActor> for VMActorCacheUpdater {
    async fn get_actor_refs(&self) -> Result<Vec<RemoteActorRef<VMActor>>> {
        let mut agent_actors_lookup = RemoteActorRef::<VMActor>::lookup_all(VM);
        let mut actor_ref_vec = Vec::new();

        while let Some(agent_actor) = agent_actors_lookup.try_next().await? {
            actor_ref_vec.push(agent_actor);
        }

        Ok(actor_ref_vec)
    }

    async fn on_update(&self, actor_ref: &RemoteActorRef<VMActor>, previous_value: Option<CachedVMActor>) -> Result<CachedVMActor, Report> {
        let output_actor_ref = match previous_value {
            Some(value) => value.actor_ref,
            _ => actor_ref.clone(),
        };

        Ok(CachedVMActor {
            actor_ref: output_actor_ref,
            metadata: actor_ref.ask(&GetVMInfo {vmid: None}).await?
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
            agent_actor_cache: ActorCache::new(actor_ref.clone(), AgentActorCacheUpdater)?,
            vm_actor_cache: ActorCache::new(actor_ref, VMActorCacheUpdater)?
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

        //self.agent_actor_cache.info().await;
        info!(vm_actor_cache=?self.vm_actor_cache.data_cache, "vm actor cache");

        self.agent_actor_cache.on_link_died(id).await;
        self.vm_actor_cache.on_link_died(id).await;

        info!(vm_actor_cache=?self.vm_actor_cache.data_cache, "vm actor cache");
        //self.agent_actor_cache.info().await;

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

        for agent in self.agent_actor_cache.data_cache.iter() {
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
