use std::{marker::PhantomData, sync::Arc, time::Duration};

use ahash::AHashMap;
use async_trait::async_trait;
use kameo::{actor, prelude::*};
use tokio::{sync::{Mutex, MutexGuard}, task::JoinHandle};
use stable_eyre::{Report, Result, eyre::eyre};
use tracing::{info, trace};

// the selfs here are because
#[async_trait]
pub trait ActorCacheUpdater<ChildActor: Actor + RemoteActor, Data: Clone + Send + Sync + 'static>: Sync + Send + Copy + 'static {
    // todo: this could probably be better if it was an iterator, but I am lazy and don't want to right now.
    async fn get_actor_refs(&self) -> Result<Vec<RemoteActorRef<ChildActor>>>;
    async fn on_update(&self, actor_ref: &RemoteActorRef<ChildActor>, previous_value: Option<Data>) -> Result<Data, Report>;
}


// todo:
// I (caleb) do not like the way this is written.
// I worry the Arc<Mutex<T>> is going to result in contention and issues, when we have large numbers of VMs.
//
// The contention will be caused by the fact that we do a ping to check the status of each VM/agent once per second.
// If we are running 1000s of VMs in the future, one hashmap to store all of that is eventually going to have latency problems
// Especially since the hashmap is also used whenever a user wants to make a change or things change in the swarm.
//
// I am leaving it this way because I just wanted to get things working. We may need to change the way the data is stored in the future.
// My suggestions are either replacing it with a concurrent map, or using a RwLock. Either one might help, but I am not dealing with it now.
#[derive(Debug)]
pub struct ActorCache<ChildActor: Actor + RemoteActor, Data: Clone + Send + Sync + 'static> {
    data_cache: Arc<Mutex<AHashMap<ActorId, Data>>>,
    keepalive_tasks: Arc<Mutex<AHashMap<ActorId, JoinHandle<()>>>>,
    actor_finder: Option<JoinHandle<()>>,

    child_actor_type: PhantomData<ChildActor>
}

// todo: impl Drop to automatically kill all the keepalive_tasks and the actor_finder task.

impl<ChildActor: Actor + RemoteActor, Data: Clone + Send + Sync + 'static> ActorCache<ChildActor, Data> {
    pub fn new(
        updater: impl ActorCacheUpdater<ChildActor, Data>
    ) -> Result<Self, Report> {

        let data_cache = Arc::new(Mutex::new(AHashMap::new()));
        let keepalive_tasks = Arc::new(Mutex::new(AHashMap::new()));

        let actor_cache = ActorCache {
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

    pub async fn lock_data_cache(&self) -> MutexGuard<'_, AHashMap<ActorId, Data>> {
        self.data_cache.lock().await
    }

    // todo: this needs a refactor because holy crap the indention.
    fn start_actor_finder(
        &self,
        updater: impl ActorCacheUpdater<ChildActor, Data>
    ) {
        let keepalive_tasks_clone = Arc::clone(&self.keepalive_tasks);
        let data_cache_clone = Arc::clone(&self.data_cache);

        tokio::spawn(async move {
            loop {
                let _ = Self::actor_finder(
                    Arc::clone(&keepalive_tasks_clone),
                    Arc::clone(&data_cache_clone),
                    updater
                );
            }
        });
    }

    async fn actor_finder(
        keepalive_tasks: Arc<Mutex<AHashMap<ActorId, JoinHandle<()>>>>,
        data_cache: Arc<Mutex<AHashMap<ActorId, Data>>>,
        updater: impl ActorCacheUpdater<ChildActor, Data>
    ) -> Result<(), Report> {
        let actor_refs = updater.get_actor_refs().await?;

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
                    ).await;
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
        updater: impl ActorCacheUpdater<ChildActor, Data>
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));

        loop {
            let actor_id = actor_ref.id();

            let mut previous_value_option = None;



            let locked_data_cache = data_cache.lock().await;

            if let Some(data_ref) = locked_data_cache.get(&actor_id) {
                previous_value_option = Some(data_ref.clone());
            }

            // done to very explicility make sure it does not stay locked.
            drop(locked_data_cache);



            if let Ok(update) = updater.on_update(&actor_ref, previous_value_option).await {
                data_cache.lock().await.insert(
                    actor_id,
                    update.clone()
                );
            }

            interval.tick().await;
        }
    }

    /*
     /// todo: implement display
    async fn print_agent_caches(&self) {
        let keepalives = self.agent_actor_keepalive_tasks.lock().await;
        let cache = self.agent_actor_cache.lock().await;

        info!("agent actor cache data");
        for keepalive in keepalives.iter() {
            info!("keepalive: {keepalive:?}");
        }
        for actor in cache.iter() {
            info!("actor: {actor:?}");
        }
    }
    */
}
