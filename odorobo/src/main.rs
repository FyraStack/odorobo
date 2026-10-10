#![allow(unknown_lints)] // Supports Clippy releases before `unused_async_trait_impl` was introduced.
#![allow(clippy::unused_async_trait_impl)] // Kameo requires async trait methods even when a handler has no await points.

pub mod actors;
mod ch_driver;
pub mod cluster_state;

pub mod config;
pub mod http_api;
mod manifest;
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
use crate::cluster_state::{ClusterStateStore, StateStore, TlsConfig};
use crate::config::Config;
use crate::utils::actor_names::{AGENT, HTTP_API_SERVER, SCHEDULER};
use crate::utils::{connect_to_swarm, init};

fn main() -> Result<()> {
    let config = Config::init();
    let _sentry_guard = init_sentry(config.sentry_dsn.as_deref());

    init(Some("odorobo"))?;
    let term = utils::lockfile::register_termsigs()?;
    let _lock = utils::lockfile::init_lockfile(&config).map_err(|e| {
        eyre!("init lockfile failed (note: the `no_lockfile` option can skip this)").wrap_err(e)
    })?;

    mainloop(&term, config)
}

fn init_sentry(dsn: Option<&str>) -> Option<sentry::ClientInitGuard> {
    let dsn = dsn?;
    let mut options = sentry::ClientOptions::default();
    options.release = sentry::release_name!();
    options.send_default_pii = false;

    let guard = sentry::init((dsn, options));
    tracing::info!("Sentry error reporting enabled");
    Some(guard)
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

    let endpoints = config.get_etcd_endpoints();
    if endpoints.is_empty() {
        return Err(eyre!("at least one etcd endpoint is required"));
    }
    if config.etcd_username.is_some() != config.etcd_password.is_some() {
        return Err(eyre!(
            "etcd username and password must be configured together"
        ));
    }
    let timeout_ms = config.etcd_timeout_ms.unwrap_or(5_000);
    if timeout_ms == 0 {
        return Err(eyre!("etcd timeout must be greater than zero"));
    }
    let retries = config.etcd_retries.unwrap_or(3);
    if retries == 0 {
        return Err(eyre!("etcd retries must be greater than zero"));
    }
    let tls = if config.etcd_tls.unwrap_or(false) {
        let ca_file = config
            .etcd_ca_file
            .clone()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| eyre!("etcd_ca_file is required when etcd TLS is enabled"))?;
        Some(TlsConfig { ca_file })
    } else {
        None
    };
    let state_store = Arc::new(
        StateStore::connect(
            &endpoints,
            config.etcd_username.as_deref(),
            config.etcd_password.as_deref(),
            tls,
            Duration::from_millis(timeout_ms),
            retries,
        )
        .await
        .map_err(|error| eyre!("unable to connect to etcd: {error}"))?,
    );
    tracing::info!("Connected to etcd for durable cluster state");
    let health = state_store.health().await;
    tracing::info!(healthy = health.healthy, message = %health.message, "Cluster state store health");

    let local_peer_id = connect_to_swarm().unwrap();
    tracing::info!(?local_peer_id, "Peer ID");

    // start agents
    let agent_actor = AgentActor::spawn((config.clone(), Arc::clone(&state_store)));
    agent_actor
        .wait_for_startup_with_result(|result| result.map_err(|error| error.to_string()))
        .await
        .map_err(|error| eyre!("agent startup failed: {error}"))?;
    agent_actor.register(AGENT).await?;

    if config.get_manager_enabled() {
        let scheduler_actor = SchedulerActor::spawn(Arc::clone(&state_store));
        scheduler_actor
            .wait_for_startup_with_result(|result| result.map_err(|error| error.to_string()))
            .await
            .map_err(|error| eyre!("scheduler startup failed: {error}"))?;
        let http_actor = HTTPActor::spawn(scheduler_actor.clone());
        http_actor.wait_for_startup().await;

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
