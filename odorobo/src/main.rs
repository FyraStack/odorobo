pub mod actors;
mod ch_driver;
pub mod config;
pub mod http_api;
pub mod messages;
pub mod networking;
pub mod types;
mod utils;

use kameo::actor::Spawn;
use stable_eyre::Result;

use crate::actors::agent_actor::AgentActor;
use crate::actors::http_actor::HTTPActor;
use crate::actors::scheduler_actor::SchedulerActor;
use crate::config::Config;
use crate::utils::actor_names::{HTTP_API_SERVER, SCHEDULER};
use crate::utils::{actor_names::AGENT, connect_to_swarm, init};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::init();

    init(Some("odorobo"))?;

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
