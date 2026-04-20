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
use tracing::{info, warn, trace, debug};
use ulid::Ulid;

use crate::actors::scheduler_actor::SchedulerActor;

#[derive(Debug)]
pub struct ActorAgentKeepalive {
    pub keepalive_task: Option<tokio::task::JoinHandle<()>>,
}

/// Periodically sends a keepalive request to all agent actors and updates their metadata.
/// todo: i think this will crash if any of the Results error. likely need to wrap it in a closure so we can just ignore errors and keep trying.
pub async fn keepalive_agents(
    actor_ref: ActorRef<SchedulerActor>,
    agent_actors: Arc<Mutex<AHashMap<ActorId, CachedAgentActor>>>,
    agent_actors_keepalives: Arc<Mutex<AHashMap<ActorId, ActorAgentKeepalive>>>
) -> Result<(), Report> {
    loop {
        let mut agent_actors_lookup = RemoteActorRef::<AgentActor>::lookup_all("agent");

        while let Some(agent_actor) = agent_actors_lookup.try_next().await? {
            tracing::trace!("UpdateAgents: agent_actor={:?}", agent_actor);

            let mut locked_agent_actors_keepalives = agent_actors_keepalives.lock().await;

            if !locked_agent_actors_keepalives.contains_key(&agent_actor.id()) {
                actor_ref.link_remote(&agent_actor).await?;

                locked_agent_actors_keepalives.insert(
                    agent_actor.id(),
                    ActorAgentKeepalive::new(agent_actor, Arc::clone(&agent_actors))
                );
            }
        }
    }
}

impl ActorAgentKeepalive {
    fn new(actor_ref: RemoteActorRef<AgentActor>, actor_cache: Arc<Mutex<AHashMap<ActorId, CachedAgentActor>>>) -> Self {
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                if let Ok(agent_status) = actor_ref.ask(&GetAgentStatus).await {
                    let mut locked_actor_cache = actor_cache.lock().await;

                    if let Some(cached_agent_actor) = locked_actor_cache.get_mut(&actor_ref.id()) {
                        cached_agent_actor.metadata = agent_status;
                    } else {
                        locked_actor_cache.insert(
                            actor_ref.id(),
                            CachedAgentActor {
                                actor_ref: actor_ref.clone(), // todo: yea we shouldnt be cloning this every time
                                metadata: agent_status
                            }
                        );
                    }
                }
                interval.tick().await;
            }
        });

        Self {
            keepalive_task: Some(task)
        }
    }
}

#[derive(Debug)]
pub struct VMAgentKeepalive {
    pub keepalive_task: Option<tokio::task::JoinHandle<()>>,
}


#[derive(Debug)]
pub struct CachedAgentActor {
    pub actor_ref: RemoteActorRef<AgentActor>,
    pub metadata: AgentStatus,
}

#[derive(Debug)]
pub struct CachedVMActor {
    pub vm_actor_ref: RemoteActorRef<VMActor>,
    pub agent_actor_ref: RemoteActorRef<AgentActor>,
    //pub metadata: VMStatus, // todo: set this type to whatever it should be.
}
