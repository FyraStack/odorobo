use crate::config::NetworkConfig;
use crate::networking::messages::{AttachTap, DetachTap};
use kameo::{message::Context, prelude::*};
use stable_eyre::Report;
use stable_eyre::eyre::eyre;
use tracing::warn;

#[derive(RemoteActor)]
pub struct NetworkAgentActor {
    pub config: NetworkConfig,
}

impl Actor for NetworkAgentActor {
    type Args = NetworkConfig;
    type Error = Report;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        warn!(
            target_os = std::env::consts::OS,
            "host networking management is only supported on Linux"
        );

        Ok(Self { config: args })
    }
}

impl Message<AttachTap> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: AttachTap,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Err(eyre!(
            "cannot attach TAP {} for VM {}: host networking management is only supported on Linux",
            msg.tap_name,
            msg.vmid
        ))
    }
}

impl Message<DetachTap> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: DetachTap,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Err(eyre!(
            "cannot detach TAP {} for VM {}: host networking management is only supported on Linux",
            msg.tap_name,
            msg.vmid
        ))
    }
}
