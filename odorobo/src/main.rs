#![allow(unknown_lints)] // Supports Clippy releases before `unused_async_trait_impl` was introduced.
#![allow(clippy::unused_async_trait_impl)] // Kameo requires async trait methods even when a handler has no await points.

pub mod actors;
mod ch_driver;
pub mod config;
pub mod http_api;
mod manifest;
pub mod messages;
pub mod networking;
pub mod types;
mod utils;
mod vmm_process;
mod vsock_cid;

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

/// a little longer than the agent's own VM shutdown timeout, so it gets to finish
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(75);

fn mainloop(term: &Arc<AtomicBool>, config: Config) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| eyre!("can't build tokio: {err}"))?;
    let result = runtime.block_on(inner_main(config, Arc::clone(term)));
    // don't hang on stray tasks once the actors are done
    runtime.shutdown_timeout(Duration::from_secs(5));
    if term.load(Ordering::Relaxed) {
        stable_eyre::eyre::bail!("Exit due to termination signal");
    }
    result
}

async fn wait_for_term(term: &AtomicBool) {
    while !term.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn inner_main(config: Config, term: Arc<AtomicBool>) -> Result<()> {
    tracing::info!("Starting odorobo");

    let local_peer_id = connect_to_swarm(std::path::Path::new(config.get_peer_key_path()))?;
    tracing::info!(?local_peer_id, "Peer ID");

    // start agents
    let agent_actor = AgentActor::spawn(config.clone());
    agent_actor.register(AGENT).await?;

    let manager = if config.get_manager_enabled() {
        let scheduler_actor = SchedulerActor::spawn(());
        let http_actor = HTTPActor::spawn(scheduler_actor.clone());

        scheduler_actor.register(SCHEDULER).await?;
        http_actor.register(HTTP_API_SERVER).await?;
        Some((scheduler_actor, http_actor))
    } else {
        None
    };

    let run = async {
        if let Some((scheduler_actor, http_actor)) = &manager {
            scheduler_actor.wait_for_shutdown().await;
            http_actor.wait_for_shutdown().await;
        }
        agent_actor.wait_for_shutdown().await;
    };
    tokio::select! {
        () = run => return Ok(()),
        () = wait_for_term(&term) => {}
    }

    // stop gracefully so the agent reaps its VMMs and marks their vsock leases
    // stopped. just dropping the runtime kills them without recording that
    tracing::info!("termination signal received, shutting down");
    let shutdown = async {
        if let Some((scheduler_actor, http_actor)) = &manager {
            _ = http_actor.stop_gracefully().await;
            _ = scheduler_actor.stop_gracefully().await;
        }
        if let Err(error) = agent_actor.stop_gracefully().await {
            tracing::warn!(?error, "failed to stop agent actor");
        }
        agent_actor.wait_for_shutdown().await;
    };
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, shutdown)
        .await
        .is_err()
    {
        tracing::warn!("timed out waiting for the agent to stop");
    }
    Ok(())
}
