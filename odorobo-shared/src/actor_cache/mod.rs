use std::{marker::PhantomData, sync::Arc, time::Duration};

use ahash::AHashMap;
use async_trait::async_trait;
use kameo::{actor, prelude::*};
use tokio::{sync::Mutex, task::JoinHandle};
use stable_eyre::{Report, Result, eyre::eyre};
use tracing::{info, trace};

pub trait DataTrait: Copy + Send + Sync + 'static {}

#[async_trait]
pub trait ActorCacheUpdater<ParentActor: Actor + RemoteActor, ChildActor: Actor + RemoteActor, Data: DataTrait>: Sync + Send + Copy + Drop + 'static {
    async fn get_actor_refs(&self, actor_ref: ActorRef<ParentActor>) -> Vec<RemoteActorRef<ChildActor>>;
    async fn on_update(&self, actor_ref: &RemoteActorRef<ChildActor>) -> Result<Data, Report>;
}

#[derive(Debug)]
struct ActorCache<ParentActor: Actor + RemoteActor, ChildActor: Actor + RemoteActor, Data: DataTrait> {
    parent_actor_ref: ActorRef<ParentActor>,
    pub data_cache: Arc<Mutex<AHashMap<ActorId, Data>>>,
    pub keepalive_tasks: Arc<Mutex<AHashMap<ActorId, JoinHandle<()>>>>,
    actor_finder: Option<JoinHandle<()>>,

    child_actor_type: PhantomData<ChildActor>
}

impl<ParentActor: Actor + RemoteActor, ChildActor: Actor + RemoteActor, Data: DataTrait> ActorCache<ParentActor, ChildActor, Data> {
    fn new(
        actor_ref: ActorRef<ParentActor>,
        updater: impl ActorCacheUpdater<ParentActor, ChildActor, Data>
    ) -> Result<Self, Report> {

        let data_cache = Arc::new(Mutex::new(AHashMap::new()));
        let keepalive_tasks = Arc::new(Mutex::new(AHashMap::new()));

        let actor_cache = ActorCache {
            parent_actor_ref: actor_ref,
            data_cache: data_cache,
            keepalive_tasks: keepalive_tasks,
            actor_finder: None,

            child_actor_type: PhantomData
        };

        actor_cache.start_actor_finder(updater);

        Ok(actor_cache)
    }

    /// run this function inside of the on_link_died of the ParentActor

    pub async fn on_link_died(
        &self,
        id: ActorId
    ) {
        //self.print_agent_caches().await;

        info!("removing agent actor from cache {id:?}");

        if let Some(actor_keepalive_task) = self.keepalive_tasks.lock().await.remove(&id) {
            trace!("Aborting keepalive task for agent {id:?}");
            actor_keepalive_task.abort();
        };

        self.data_cache.lock().await.remove(&id);

        //self.print_agent_caches().await;
    }

    // todo: this needs a refactor because holy crap the indention.
    fn start_actor_finder(
        &self,
        updater: impl ActorCacheUpdater<ParentActor, ChildActor, Data>
    ) {
        let parent_actor_ref_clone = self.parent_actor_ref.clone();
        let keepalive_tasks_clone = Arc::clone(&self.keepalive_tasks);
        let data_cache_clone = Arc::clone(&self.data_cache);

        tokio::spawn(async move {
            loop {
                let _ = Self::actor_finder(
                    parent_actor_ref_clone.clone(),
                    Arc::clone(&keepalive_tasks_clone),
                    Arc::clone(&data_cache_clone),
                    updater
                );
            }
        });
    }

    async fn actor_finder(
        parent_actor_ref: ActorRef<ParentActor>,
        keepalive_tasks: Arc<Mutex<AHashMap<ActorId, JoinHandle<()>>>>,
        data_cache: Arc<Mutex<AHashMap<ActorId, Data>>>,
        updater: impl ActorCacheUpdater<ParentActor, ChildActor, Data>
    ) -> Result<(), Report> {
        let actor_refs = updater.get_actor_refs(parent_actor_ref).await;

        for actor_ref in actor_refs {
            tracing::trace!("UpdateAgents: agent_actor={:?}", actor_ref);

            let mut locked_agent_actors_keepalives = keepalive_tasks.lock().await;

            if !locked_agent_actors_keepalives.contains_key(&actor_ref.id()) {
                actor_ref.link_remote(&actor_ref).await?;

                let actor_ref_clone = actor_ref.clone();
                let data_cache_clone = Arc::clone(&data_cache);
                let updater_task = tokio::spawn(async move {
                    Self::updater_task(
                        actor_ref_clone,
                        data_cache_clone,
                        updater
                    );
                });

                locked_agent_actors_keepalives.insert(
                    actor_ref.id(),
                    updater_task
                );
            }
        }

        Ok(())
    }

    async fn updater_task(
        actor_ref: RemoteActorRef<ChildActor>,
        data_cache:  Arc<Mutex<AHashMap<ActorId, Data>>>,
        updater: impl ActorCacheUpdater<ParentActor, ChildActor, Data>
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));

        loop {
            if let Ok(update) = updater.on_update(&actor_ref).await {
                data_cache.lock().await.insert(
                    actor_ref.id(),
                    update
                );
            }

            interval.tick().await;
        }
    }
}
