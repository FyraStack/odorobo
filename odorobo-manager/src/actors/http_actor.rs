use kameo::prelude::*;
use tracing::{error, info, warn};
//use odorobo_shared::odorobo::server_actor::ServerActor;
use odorobo_agent::actor::AgentActor;
use odorobo_shared::messages::create_vm::*;
use odorobo_shared::messages::debug::PanicAgent;
use stable_eyre::{Report, Result};
const EXTERNAL_HTTP_ADDRESS: &str = "0.0.0.0:3000";
const EXTERNAL_HTTP_URL: &str = "http://localhost:3000"; // TODO: mak
/// HTTP REST API service
#[derive(RemoteActor)]
pub struct HTTPActor;

impl Actor for HTTPActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // run the HTTP API
        tokio::spawn(async move {
            tracing::info!("Starting HTTP server on {EXTERNAL_HTTP_URL}");
            let listener = tokio::net::TcpListener::bind(EXTERNAL_HTTP_ADDRESS)
                .await
                .unwrap();
            axum::serve(listener, crate::api::build()).await.unwrap();
        });

        Ok(Self)
    }
}
