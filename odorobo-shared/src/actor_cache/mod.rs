use std::{marker::PhantomData, sync::Arc, time::Duration};

use ahash::AHashMap;
use async_trait::async_trait;
use kameo::{prelude::*};
use tokio::{sync::{Mutex, MutexGuard}, task::JoinHandle};
use stable_eyre::{Report, Result};
use tracing::{info, trace};

use std::fmt;

// future refactor TODO because I don't know how to do it now.
// The best way to make this would be that you crate a struct with #[derive(ActorCache)]
// and then you impl ActorCache with setting the ChildActor and Data as types similar to https://github.com/tqwewe/kameo/blob/1d498c0566b613b9afe6d54965c4b191c84432e0/src/actor.rs#L122
// you could then just implement these get_actor_ref and on_update methods during that.
// we also would likely want default methods that just let you lookup_all for a specific actor string.
// you would also likely want to change get_actor_refs to return an iterator during this if you are doing this anyway.
// the problem is to do this you need to write a derive macro and I have no clue how to do that.
// and learning that now is not something i should spend tiem doing.
// so unfortunately instead I have to use self inside of the ActorCacheUpdater trait to make it work.
// Which I hate
// this would also make it where we dont need two structs, one for data and one for the update function hooks.
// I thnk it would also likely make a lot of the generic types simpler since hopefully their trait bounds would only be in one place.


#[async_trait]
pub trait ActorCacheUpdater<ChildActor: Actor + RemoteActor, Data: Clone + Send + Sync + 'static + fmt::Debug>: Sync + Send + Copy + 'static {
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
pub struct ActorCache<ParentActor: Actor + RemoteActor, ChildActor: Actor + RemoteActor, Data: Clone + Send + Sync + 'static + fmt::Debug> {
    parent_actor_ref: ActorRef<ParentActor>,
    data_cache: Arc<Mutex<AHashMap<ActorId, Data>>>,
    keepalive_tasks: Arc<Mutex<AHashMap<ActorId, JoinHandle<()>>>>,
    actor_finder: Option<JoinHandle<()>>,

    child_actor_type: PhantomData<ChildActor>
}

// todo: impl Drop to automatically kill all the keepalive_tasks and the actor_finder task.

impl<ParentActor: Actor + RemoteActor, ChildActor: Actor + RemoteActor, Data: Clone + Send + Sync + 'static + fmt::Debug> ActorCache<ParentActor, ChildActor, Data> {
    pub fn new(
        parent_actor_ref: ActorRef<ParentActor>,
        updater: impl ActorCacheUpdater<ChildActor, Data>
    ) -> Result<Self, Report> {

        let data_cache = Arc::new(Mutex::new(AHashMap::new()));
        let keepalive_tasks = Arc::new(Mutex::new(AHashMap::new()));

        let actor_cache = ActorCache {
            parent_actor_ref: parent_actor_ref.clone(),
            data_cache: data_cache,
            keepalive_tasks: keepalive_tasks,
            actor_finder: None,

            child_actor_type: PhantomData
        };

        actor_cache.start_actor_finder(parent_actor_ref, updater);

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

    fn start_actor_finder(
        &self,
        parent_actor_ref: ActorRef<ParentActor>,
        updater: impl ActorCacheUpdater<ChildActor, Data>
    ) {
        let keepalive_tasks_clone = Arc::clone(&self.keepalive_tasks);
        let data_cache_clone = Arc::clone(&self.data_cache);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                info!("running actor_finder");
                let _ = Self::actor_finder(
                    parent_actor_ref.clone(),
                    Arc::clone(&keepalive_tasks_clone),
                    Arc::clone(&data_cache_clone),
                    updater
                ).await;

                interval.tick().await;
            }
        });
    }

    async fn actor_finder(
        parent_actor_ref: ActorRef<ParentActor>,
        keepalive_tasks: Arc<Mutex<AHashMap<ActorId, JoinHandle<()>>>>,
        data_cache: Arc<Mutex<AHashMap<ActorId, Data>>>,
        updater: impl ActorCacheUpdater<ChildActor, Data>
    ) -> Result<(), Report> {
        let actor_refs = updater.get_actor_refs().await?;

        info!("actor_finder actor_refs: {actor_refs:?}");

        for actor_ref in actor_refs {
            tracing::trace!("UpdateAgents: agent_actor={:?}", actor_ref);

            let mut locked_agent_actors_keepalives = keepalive_tasks.lock().await;

            if !locked_agent_actors_keepalives.contains_key(&actor_ref.id()) {

                parent_actor_ref.link_remote(&actor_ref).await?;

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

    // todo: get cappy to tell me how you are supposed to do this properly
    pub async fn info(&self) {
        let keepalives = self.keepalive_tasks.lock().await;
        let cache = self.data_cache.lock().await;

        info!("agent actor cache data");
        for keepalive in keepalives.iter() {
            info!("keepalive: {keepalive:?}");
        }
        for data in cache.iter() {
            info!("data: {data:?}");
        }
    }
}
