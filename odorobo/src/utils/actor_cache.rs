use std::{marker::PhantomData, sync::Arc, time::Duration};

use async_trait::async_trait;
use dashmap::DashMap;
use kameo::{prelude::*};
use tokio::task::JoinHandle;
use stable_eyre::{Report, Result};
use tracing::{info, instrument, trace};

use std::fmt;

// TODO: refactor to use derive macro, but I (caleb) don't know how to write a derive macro.
//
// The best way to write this would be using a derive similar to kameo
// so you would create a struct with #[derive(ActorCache)].
// Then you would set the types and then write get_actor_refs and on_update.
// This would combine everything into one struct and make it a lot easier to work with.
//
// similar to: https://github.com/tqwewe/kameo/blob/1d498c0566b613b9afe6d54965c4b191c84432e0/src/actor.rs#L122
//
//
// other things we might want:
// - a default get_actor_refs that just finds all actor_refs with a specific actor string.
// - change get_actor_refs to use an iterator


#[async_trait]
pub trait ActorCacheUpdater<ChildActor: Actor + RemoteActor, Data: Clone + Send + Sync + 'static + fmt::Debug>: Sync + Send + Copy + 'static {
    // todo: this could probably be better if it was an iterator, but I am lazy and don't want to right now.
    async fn get_actor_refs(&self) -> Result<Vec<RemoteActorRef<ChildActor>>>;
    async fn on_update(&self, actor_ref: &RemoteActorRef<ChildActor>, previous_value: Option<Data>) -> Result<Data, Report>;
}


#[derive(Debug)]
pub struct ActorCache<ParentActor: Actor + RemoteActor, ChildActor: Actor + RemoteActor, Data: Clone + Send + Sync + 'static + fmt::Debug> {
    parent_actor_ref: ActorRef<ParentActor>,
    pub data_cache: Arc<DashMap<ActorId, Data>>,
    keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,
    actor_finder: Option<JoinHandle<()>>,

    child_actor_type: PhantomData<ChildActor>
}

// todo: impl Drop to automatically kill all the keepalive_tasks and the actor_finder task.

impl<ParentActor: Actor + RemoteActor, ChildActor: Actor + RemoteActor, Data: Clone + Send + Sync + 'static + fmt::Debug> ActorCache<ParentActor, ChildActor, Data> {
    pub fn new(
        parent_actor_ref: ActorRef<ParentActor>,
        updater: impl ActorCacheUpdater<ChildActor, Data>
    ) -> Result<Self, Report> {

        let data_cache = Arc::new(DashMap::new());
        let keepalive_tasks = Arc::new(DashMap::new());

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
        info!("removing agent actor from cache {id:?}");

        if let Some(actor_keepalive_task) = self.keepalive_tasks.remove(&id) {
            trace!("Aborting keepalive task for agent {id:?}");
            actor_keepalive_task.1.abort();
        };

        self.data_cache.remove(&id);
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
        keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,
        data_cache: Arc<DashMap<ActorId, Data>>,
        updater: impl ActorCacheUpdater<ChildActor, Data>
    ) -> Result<(), Report> {
        let actor_refs = updater.get_actor_refs().await?;

        info!(?actor_refs, "running actor_finder");

        for actor_ref in actor_refs {
            if !keepalive_tasks.contains_key(&actor_ref.id()) {
                trace!(?actor_ref, "starting updater_task");

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

                keepalive_tasks.insert(
                    actor_ref.id(),
                    updater_task
                );
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn updater_task(
        actor_ref: RemoteActorRef<ChildActor>,
        data_cache:  Arc<DashMap<ActorId, Data>>,
        updater: impl ActorCacheUpdater<ChildActor, Data>
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));

        loop {
            let actor_id = actor_ref.id();

            let mut previous_value_option = None;



            if let Some(data_ref) = data_cache.get(&actor_id) {
                previous_value_option = Some(data_ref.clone());
            }


            if let Ok(update) = updater.on_update(&actor_ref, previous_value_option).await {
                data_cache.insert(
                    actor_id,
                    update.clone()
                );
            }

            interval.tick().await;
        }
    }
}
