pub mod actors;
pub mod http_api;
pub mod networking;
mod ch_driver;
mod utils;
pub mod messages;
pub mod config;
pub mod types;

use std::fs;

use clap::Parser;
use kameo::actor::Spawn;
use stable_eyre::Result;

use crate::actors::http_actor::HTTPActor;
use crate::actors::scheduler_actor::SchedulerActor;
use crate::config::Config;
use crate::utils::actor_names::{HTTP_API_SERVER, SCHEDULER};
use crate::utils::{actor_names::AGENT, connect_to_swarm, init};
use crate::actors::agent_actor::AgentActor;

#[tokio::main]
async fn main() -> Result<()> {
    let cli_config = config::CliConfig::parse();
    // TODO: ask infra team where they want this on the box
    let config: Config = if let Ok(file) = fs::File::open("config.json") {
        serde_json::from_reader(file).expect("unable to parse config.json")
    } else {
        Config::default()
    };

    init(Some("odorobo"))?;

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
