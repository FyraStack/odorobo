use std::{marker::PhantomData, sync::Arc, time::Duration};

use async_trait::async_trait;
use dashmap::DashMap;
use kameo::prelude::*;
use stable_eyre::{Report, Result};
use tokio::task::JoinHandle;
use tracing::{info, instrument, trace};

use std::fmt;

#[async_trait]
pub trait ActorCacheUpdater<
    ChildActor: Actor + RemoteActor,
    Data: Clone + Send + Sync + 'static + fmt::Debug,
>: Sync + Send + Copy + 'static
{
    async fn get_actor_refs(&self) -> Result<Vec<RemoteActorRef<ChildActor>>>;
    async fn on_update(
        &self,
        actor_ref: &RemoteActorRef<ChildActor>,
        previous_value: Option<Data>,
    ) -> Result<Data, Report>;
}

#[derive(Debug)]
pub struct ActorCache<
    ParentActor: Actor + RemoteActor,
    ChildActor: Actor + RemoteActor,
    Data: Clone + Send + Sync + 'static + fmt::Debug,
> {
    #[expect(
        dead_code,
        reason = "keeps the parent actor reference alive for cache-owned tasks"
    )]
    parent_actor_ref: ActorRef<ParentActor>,
    pub data_cache: Arc<DashMap<ActorId, Data>>,
    keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,
    actor_finder: JoinHandle<()>,

    child_actor_type: PhantomData<ChildActor>,
}

impl<
    ParentActor: Actor + RemoteActor,
    ChildActor: Actor + RemoteActor,
    Data: Clone + Send + Sync + 'static + fmt::Debug,
> ActorCache<ParentActor, ChildActor, Data>
{
    pub fn new<Updater>(parent_actor_ref: ActorRef<ParentActor>, updater: Updater) -> Self
    where
        Updater: ActorCacheUpdater<ChildActor, Data>,
    {
        let data_cache = Arc::new(DashMap::new());
        let keepalive_tasks = Arc::new(DashMap::new());

        let actor_finder = Self::start_actor_finder(
            parent_actor_ref.clone(),
            Arc::clone(&keepalive_tasks),
            Arc::clone(&data_cache),
            updater,
        );

        Self {
            parent_actor_ref,
            data_cache,
            keepalive_tasks,
            actor_finder,
            child_actor_type: PhantomData,
        }
    }

    /// run this function inside of the `on_link_died` of the `ParentActor`
    pub fn on_link_died(&self, id: ActorId) {
        info!("removing agent actor from cache {id:?}");

        if let Some(actor_keepalive_task) = self.keepalive_tasks.remove(&id) {
            trace!("Aborting keepalive task for agent {id:?}");
            actor_keepalive_task.1.abort();
        }

        self.data_cache.remove(&id);
    }

    fn start_actor_finder<Updater>(
        parent_actor_ref: ActorRef<ParentActor>,
        keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,
        data_cache: Arc<DashMap<ActorId, Data>>,
        updater: Updater,
    ) -> JoinHandle<()>
    where
        Updater: ActorCacheUpdater<ChildActor, Data>,
    {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                _ = Self::actor_finder(
                    parent_actor_ref.clone(),
                    Arc::clone(&keepalive_tasks),
                    Arc::clone(&data_cache),
                    updater,
                )
                .await;

                interval.tick().await;
            }
        })
    }

    async fn actor_finder<Updater>(
        parent_actor_ref: ActorRef<ParentActor>,
        keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,
        data_cache: Arc<DashMap<ActorId, Data>>,
        updater: Updater,
    ) -> Result<(), Report>
    where
        Updater: ActorCacheUpdater<ChildActor, Data>,
    {
        let actor_refs = updater.get_actor_refs().await?;

        info!(?actor_refs, "running actor_finder");

        for actor_ref in actor_refs {
            if !keepalive_tasks.contains_key(&actor_ref.id()) {
                trace!(?actor_ref, "starting updater_task");

                parent_actor_ref.link_remote(&actor_ref).await?;

                let actor_ref_clone = actor_ref.clone();
                let data_cache_clone = Arc::clone(&data_cache);
                let updater_task = tokio::spawn(async move {
                    Self::updater_task(actor_ref_clone, data_cache_clone, updater).await;
                });

                keepalive_tasks.insert(actor_ref.id(), updater_task);
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn updater_task<Updater>(
        actor_ref: RemoteActorRef<ChildActor>,
        data_cache: Arc<DashMap<ActorId, Data>>,
        updater: Updater,
    ) where
        Updater: ActorCacheUpdater<ChildActor, Data>,
    {
        let mut interval = tokio::time::interval(Duration::from_secs(1));

        loop {
            let actor_id = actor_ref.id();

            let mut previous_value_option = None;

            if let Some(data_ref) = data_cache.get(&actor_id) {
                previous_value_option = Some(data_ref.clone());
            }

            if let Ok(update) = updater.on_update(&actor_ref, previous_value_option).await {
                data_cache.insert(actor_id, update.clone());
            }

            interval.tick().await;
        }
    }
}

impl<
    ParentActor: Actor + RemoteActor,
    ChildActor: Actor + RemoteActor,
    Data: Clone + Send + Sync + 'static + fmt::Debug,
> Drop for ActorCache<ParentActor, ChildActor, Data>
{
    fn drop(&mut self) {
        self.actor_finder.abort();
        for entry in self.keepalive_tasks.iter() {
            entry.value().abort();
        }
    }
}
