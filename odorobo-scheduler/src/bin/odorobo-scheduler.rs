use kameo::prelude::*;
use odorobo_shared::utils::DynError;
use odorobo_scheduler::scheduler_actor::SchedulerActor;
use odorobo_shared::connect_to_swarm;

#[tokio::main]
async fn main() -> Result<(), DynError> {
    let _local_peer_id = connect_to_swarm()?;

    let actor_ref = SchedulerActor::spawn(SchedulerActor {});
    actor_ref.register("scheduler").await?;

    actor_ref.wait_for_shutdown().await;

    Ok(())
}