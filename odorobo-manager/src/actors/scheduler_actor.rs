use kameo::prelude::*;
use tower_http::classify::SharedClassifier;
use tracing::{error, info, warn};
//use odorobo_shared::odorobo::server_actor::ServerActor;
use odorobo_agent::actor::AgentActor;
use odorobo_shared::messages::create_vm::*;
use odorobo_shared::messages::debug::PanicAgent;
use stable_eyre::{Report, Result};

#[derive(RemoteActor)]
pub struct SchedulerActor;

impl Actor for SchedulerActor {
    type Args = ();
    type Error = Report;

    async fn on_start(state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let peer_id = actor_ref.id().peer_id().unwrap().clone();

        info!("Actor started! Scheduler peer id: {peer_id}");

        let mut agent_actor: Option<RemoteActorRef<AgentActor>> = None;

        loop {
            let agent_actor_option = RemoteActorRef::<AgentActor>::lookup("agent").await?;

            let Some(agent_actor_in_loop) = agent_actor_option else {
                continue;
            };

            agent_actor = Some(agent_actor_in_loop);
            break;
        }

        let agent_actor = agent_actor.unwrap();

        let agent_actor_peer_id = agent_actor.id().peer_id().unwrap().clone();

        info!("Agent actor peer id: {agent_actor_peer_id}");



        // let reply = agent_actor
        //     .ask(&CreateVM {
        //         vm_id: Default::default(),
        //         config: Default::default(),
        //     })
        //     .await?;

        // info!(?reply, "Created VM Reply");

        // tokio::time::sleep(std::time::Duration::from_secs(10)).await;

        // warn!("Panicking Agent");

        // agent_actor.tell(&PanicAgent).send()?;

        // error!("Agent has been panicked.");

        Ok(Self)
    }
}
