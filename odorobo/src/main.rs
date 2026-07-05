pub mod actors;
mod ch_driver;
pub mod config;
pub mod http_api;
pub mod messages;
pub mod networking;
pub mod types;
mod utils;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;
use kameo::actor::Spawn;
use stable_eyre::Result;

use crate::actors::agent_actor::AgentActor;
use crate::actors::http_actor::HTTPActor;
use crate::actors::scheduler_actor::SchedulerActor;
use crate::config::Config;
use crate::utils::actor_names::{HTTP_API_SERVER, SCHEDULER};
use crate::utils::{actor_names::AGENT, connect_to_swarm, init};

fn main() -> Result<()> {
    let cli_config = config::CliConfig::parse();
    // TODO: ask infra team where they want this on the box
    let config: Config = if let Ok(file) = fs::File::open("config.json") {
        serde_json::from_reader(file).expect("unable to parse config.json")
    } else {
        Config::default()
    };

    init(Some("odorobo"))?;
    let term = utils::lockfile::register_termsigs()?;
    let _lock = utils::lockfile::init_lockfile()
        .map_err(|e| stable_eyre::eyre::eyre!("cannot init lockfile").wrap_err(e))?;

    mainloop(term, cli_config, config)
}

fn mainloop(term: Arc<AtomicBool>, cli_config: config::CliConfig, config: Config) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can't build tokio");
    let handle = runtime.spawn(inner_main(cli_config, config));

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

async fn inner_main(cli_config: config::CliConfig, config: Config) -> Result<()> {
    tracing::info!(?config, "Starting odorobo");

    let local_peer_id = connect_to_swarm().unwrap();
    tracing::info!(?local_peer_id, "Peer ID");

    // start agents
    let agent_actor = AgentActor::spawn(config.clone());
    agent_actor.register(AGENT).await?;

    if cli_config.manager_enabled {
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
