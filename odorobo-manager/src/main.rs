use kameo::prelude::*;
use odorobo_manager::scheduler_actor::SchedulerActor;
use odorobo_shared::connect_to_swarm;
use stable_eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let _local_peer_id = connect_to_swarm()?;

    odorobo_shared::utils::init()?;

    let actor_ref = SchedulerActor::spawn(SchedulerActor {});
    actor_ref.register("scheduler").await?;

    actor_ref.wait_for_shutdown().await;

    Ok(())
}
