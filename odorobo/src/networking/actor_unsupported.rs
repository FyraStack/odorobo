use std::path::PathBuf;

use crate::config::NetworkConfig;
use crate::messages::network::{
    EnsureNetwork, EnsureNetworkReply, RemoveNetwork, RemoveNetworkReply,
};
use crate::networking::messages::{
    AttachNic, DetachNic, LookupNetwork, LookupNetworkReply, NetworkStatus, PublishPorts, Status,
    UnpublishPorts,
};
use kameo::{message::Context, prelude::*};
use stable_eyre::Report;
use stable_eyre::eyre::eyre;
use tracing::warn;

#[derive(RemoteActor)]
pub struct NetworkAgentActor {
    pub config: NetworkConfig,
}

impl Actor for NetworkAgentActor {
    type Args = (NetworkConfig, PathBuf);
    type Error = Report;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        warn!(
            target_os = std::env::consts::OS,
            "host networking management is only supported on Linux"
        );

        Ok(Self { config: args.0 })
    }
}

impl Message<EnsureNetwork> for NetworkAgentActor {
    type Reply = EnsureNetworkReply;

    async fn handle(
        &mut self,
        _msg: EnsureNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        EnsureNetworkReply {
            error: Some(
                "cannot ensure network: host networking management is only supported on Linux"
                    .to_owned(),
            ),
        }
    }
}

impl Message<RemoveNetwork> for NetworkAgentActor {
    type Reply = RemoveNetworkReply;

    async fn handle(
        &mut self,
        _msg: RemoveNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        RemoveNetworkReply {
            error: Some(
                "cannot remove network: host networking management is only supported on Linux"
                    .to_owned(),
            ),
        }
    }
}

impl Message<LookupNetwork> for NetworkAgentActor {
    type Reply = LookupNetworkReply;

    async fn handle(
        &mut self,
        _msg: LookupNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        LookupNetworkReply { network: None }
    }
}

impl Message<AttachNic> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: AttachNic,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Err(eyre!(
            "cannot attach TAP {} for VM {}: host networking management is only supported on Linux",
            msg.tap_name,
            msg.vmid
        ))
    }
}

impl Message<DetachNic> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: DetachNic,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Err(eyre!(
            "cannot detach TAP {} for VM {}: host networking management is only supported on Linux",
            msg.tap_name,
            msg.vmid
        ))
    }
}

impl Message<PublishPorts> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: PublishPorts,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Err(eyre!(
            "cannot publish {} port(s) for VM {}: host networking management is only supported on Linux",
            msg.ports.len(),
            msg.vmid
        ))
    }
}

impl Message<UnpublishPorts> for NetworkAgentActor {
    type Reply = Result<(), Report>;

    async fn handle(
        &mut self,
        msg: UnpublishPorts,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Err(eyre!(
            "cannot unpublish ports for VM {}: host networking management is only supported on Linux",
            msg.vmid
        ))
    }
}

impl Message<Status> for NetworkAgentActor {
    type Reply = NetworkStatus;

    async fn handle(&mut self, _msg: Status, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        NetworkStatus::default()
    }
}
