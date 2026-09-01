use kameo::prelude::*;
use stable_eyre::{Report, Result};

/// Serial-terminal WebSocket service.
///
/// Transport setup is intentionally deferred until the terminal protocol is
/// configured; the actor can still participate in the application lifecycle.
#[derive(RemoteActor)]
pub struct SerialTerminalWebsocketActor;

impl Actor for SerialTerminalWebsocketActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Self)
    }
}
