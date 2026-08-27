use kameo::prelude::*;
use stable_eyre::{Report, Result};

/// Storage service actor.
///
/// Storage backend setup is intentionally deferred until a backend is
/// configured; the actor can still participate in the application lifecycle.
#[derive(RemoteActor)]
pub struct StorageActor;

impl Actor for StorageActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Self)
    }
}
