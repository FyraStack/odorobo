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

    init(Some("odorobo"))?;
    let term = utils::lockfile::register_termsigs()?;
    let _lock = utils::lockfile::init_lockfile(&config).map_err(|e| {
        eyre!("init lockfile failed (note: the `no_lockfile` option can skip this)").wrap_err(e)
    })?;

    mainloop(term, config)
}

fn mainloop(term: Arc<AtomicBool>, config: Config) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can't build tokio");
    let handle = runtime.spawn(inner_main(config));

    loop {
        if handle.is_finished() {
            break runtime.block_on(handle).expect("cannot join main thread");
        }
        if term.load(Ordering::Relaxed) {
            handle.abort();
            stable_eyre::eyre::bail!("Exit due to termination signal");
        }
    }
}

async fn inner_main(config: Config) -> Result<()> {
    tracing::info!(?config, "Starting odorobo");

    let local_peer_id = connect_to_swarm().unwrap();
    tracing::info!(?local_peer_id, "Peer ID");

    // start agents
    let agent_actor = AgentActor::spawn(config.clone());
    agent_actor.register(AGENT).await?;

    if config.get_manager_enabled() {
        let scheduler_actor = SchedulerActor::spawn(());
        let http_actor = HTTPActor::spawn(scheduler_actor.clone());

        scheduler_actor.register(SCHEDULER).await?;
        http_actor.register(HTTP_API_SERVER).await?;

        scheduler_actor.wait_for_shutdown().await;
        http_actor.wait_for_shutdown().await;
    }

    agent_actor.wait_for_shutdown().await;

    Ok(())
}
