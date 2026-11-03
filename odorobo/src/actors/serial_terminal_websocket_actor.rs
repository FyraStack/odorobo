use kameo::prelude::*;
use stable_eyre::{Report, Result, eyre::eyre};

/// Serial-terminal WebSocket service.
///
/// Transport setup is intentionally deferred until the terminal protocol is configured.
#[derive(RemoteActor)]
pub struct SerialTerminalWebsocketActor;

impl Actor for SerialTerminalWebsocketActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Err(eyre!(
            "serial-terminal WebSocket service is not implemented"
        ))
    }
}
