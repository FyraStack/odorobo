use kameo::prelude::*;
use stable_eyre::{Report, Result};

/// HTTP REST API service
#[derive(RemoteActor)]
pub struct SerialTerminalWebsocketActor;

impl Actor for SerialTerminalWebsocketActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        todo!()
    }
}
