use kameo::prelude::*;
use tracing::{error, info, warn};
//use odorobo_shared::odorobo::server_actor::ServerActor;
use odorobo_agent::actor::AgentActor;
use odorobo_shared::messages::create_vm::*;
use odorobo_shared::messages::debug::PanicAgent;
use stable_eyre::{Report, Result};

#[derive(RemoteActor)]
pub struct SchedulerActor {}

const PING_RETURN_VALUE: &str = "pong";
const EXTERNAL_HTTP_ADDRESS: &str = "0.0.0.0:3000";

const EXTERNAL_HTTP_URL: &str = "http://localhost:3000"; // TODO: make this based on EXTERNAL_HTTP_ADDRESS. const compile time stuff is a pain.

impl Actor for SchedulerActor {
    type Args = Self;
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

        // run the HTTP API
        tokio::spawn(async move {
            tracing::info!("Starting HTTP server on {EXTERNAL_HTTP_URL}");
            let listener = tokio::net::TcpListener::bind(EXTERNAL_HTTP_ADDRESS)
                .await
                .unwrap();
            axum::serve(listener, crate::api::build()).await.unwrap();
        });



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

        Ok(state)
    }
}
