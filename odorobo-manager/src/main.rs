use clap::Parser;
use kameo::prelude::*;
use odorobo_manager::scheduler_actor::SchedulerActor;
use odorobo_shared::connect_to_swarm;
use serde::{Deserialize, Serialize};
use stable_eyre::Result;
use tracing::info;


// A config we definitely need: what is the router ip.
// TODO: talk to katherine about exactly how they want this config, since they may or may not be doing some of this, and we dont know if they want a .json for this or .env or something else.
#[derive(Serialize, Deserialize, Debug, Parser)]
struct Config {
    /// Comma-separated list of actors to enable.
    #[clap(
        env = "ODOROBO_ACTORS",
        default_value = "api,scheduler",
        value_delimiter = ','
    )]
    enabled_actors: Vec<String>,
}

// #[derive(Serialize, Deserialize, Debug)]
// struct EnabledActorsConfig {
//     /// http should probably almost always be true, but its a config just in case.
//     http: bool,
//     /// scheduler
//     scheduler: bool,
// }

#[tokio::main]
async fn main() -> Result<()> {
    let _local_peer_id = connect_to_swarm()?;

    odorobo_shared::utils::init()?;
    dotenvy::dotenv()?;
    info!("Starting odorobo-manager");

    let actor_ref = SchedulerActor::spawn(SchedulerActor {});
    actor_ref.register("scheduler").await?;

    actor_ref.wait_for_shutdown().await;

    Ok(())
}
