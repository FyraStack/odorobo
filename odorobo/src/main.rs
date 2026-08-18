pub mod actors;
mod ch_driver;

pub mod config;
pub mod http_api;
pub mod messages;
pub mod networking;
pub mod types;
mod utils;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kameo::actor::Spawn;
use stable_eyre::{Result, eyre::eyre};

use crate::actors::agent_actor::AgentActor;
use crate::actors::http_actor::HTTPActor;
use crate::actors::scheduler_actor::SchedulerActor;
use crate::config::Config;
use crate::utils::actor_names::{AGENT, HTTP_API_SERVER, SCHEDULER};
use crate::utils::{connect_to_swarm, init};
use odorobo::cluster_state::{ClusterStateStore, MemoryStateStore, StateStore, TlsConfig};

fn main() -> Result<()> {
    let config = Config::init();

    init(Some("odorobo"))?;
    let term = utils::lockfile::register_termsigs()?;
    let _lock = utils::lockfile::init_lockfile(&config).map_err(|e| {
        eyre!("init lockfile failed (note: the `no_lockfile` option can skip this)").wrap_err(e)
    })?;

    mainloop(&term, config)
}

fn mainloop(term: &Arc<AtomicBool>, config: Config) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| eyre!("can't build tokio: {err}"))?;
    let handle = runtime.spawn(inner_main(config));

    loop {
        if handle.is_finished() {
            break runtime
                .block_on(handle)
                .map_err(|err| eyre!("cannot join main thread: {err}"))?;
        }
        if term.load(Ordering::Relaxed) {
            handle.abort();
            stable_eyre::eyre::bail!("Exit due to termination signal");
        }
    }
}

async fn inner_main(config: Config) -> Result<()> {
    tracing::info!("Starting odorobo");

    let endpoints = config.etcd_endpoints.clone().unwrap_or_default();
    let tls = config.etcd_tls.unwrap_or(false).then(|| TlsConfig {
        ca_file: config.etcd_ca_file.clone().unwrap_or_default(),
    });
    let state_store = match StateStore::connect(
        &endpoints,
        config.etcd_username.as_deref(),
        config.etcd_password.as_deref(),
        tls,
        Duration::from_millis(config.etcd_timeout_ms.unwrap_or(5_000)),
        config.etcd_retries.unwrap_or(3),
    )
    .await
    {
        Ok(store) => {
            tracing::info!("Connected to etcd for durable cluster state");
            Arc::new(store)
        }
        Err(error) => {
            tracing::error!(
                ?error,
                "etcd unavailable; using in-memory state and disabling durability"
            );
            Arc::new(StateStore::Memory(MemoryStateStore::default()))
        }
    };
    let health = state_store.health().await;
    tracing::info!(healthy = health.healthy, message = %health.message, "Cluster state store health");

    let local_peer_id = connect_to_swarm().unwrap();
    tracing::info!(?local_peer_id, "Peer ID");

    // start agents
    let agent_actor = AgentActor::spawn((config.clone(), Arc::clone(&state_store)));
    agent_actor.register(AGENT).await?;

    if config.get_manager_enabled() {
        let scheduler_actor = SchedulerActor::spawn(Arc::clone(&state_store));
        let http_actor = HTTPActor::spawn(scheduler_actor.clone());

        scheduler_actor.register(SCHEDULER).await?;
        http_actor.register(HTTP_API_SERVER).await?;

        scheduler_actor.wait_for_shutdown().await;
        http_actor.wait_for_shutdown().await;
        drop(http_actor);
        drop(scheduler_actor);
    }
    drop(state_store);

    agent_actor.wait_for_shutdown().await;
    drop(agent_actor);

    Ok(())
}
