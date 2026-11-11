use crate::messages::agent::{CollectRepositoryGarbage, RepositoryGcReply};
use crate::messages::network::{
    CreateNetwork, CreateNetworkReply, DeleteNetwork, DeleteNetworkReply, GetNetwork,
    GetNetworkReply, ListNetworks, ListNetworksReply,
};
use crate::messages::vm::{
    AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
    GetConsoleHistory, GetConsoleHistoryReply, GetVMInfo, GetVMInfoReply, GetWorkloadLogs,
    GetWorkloadLogsReply, ShutdownVM, ShutdownVMReply,
};
use kameo::prelude::*;
use stable_eyre::{
    Report, Result,
    eyre::{WrapErr, eyre},
};

use super::scheduler_actor::SchedulerActor;

const EXTERNAL_HTTP_ADDRESS: &str = "0.0.0.0:3000";
/// HTTP REST API service
#[derive(RemoteActor)]
pub struct HTTPActor {
    pub scheduler: ActorRef<SchedulerActor>,
}

#[allow(clippy::unused_async_trait_impl)]
impl Actor for HTTPActor {
    type Args = ActorRef<SchedulerActor>;
    type Error = Report;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // run the HTTP API
        tokio::spawn(async move {
            tracing::info!(?EXTERNAL_HTTP_ADDRESS, "Starting HTTP server");
            let listener = tokio::net::TcpListener::bind(EXTERNAL_HTTP_ADDRESS)
                .await
                .unwrap();
            axum::serve(listener, crate::http_api::build(actor_ref))
                .await
                .unwrap();
        });

        Ok(Self { scheduler: args })
    }
}

impl Message<CreateVM> for HTTPActor {
    type Reply = Result<CreateVMReply, Report>;

    async fn handle(
        &mut self,
        msg: CreateVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to create VM via scheduler")
    }
}

impl Message<DeleteVM> for HTTPActor {
    type Reply = Result<DeleteVMReply, Report>;

    async fn handle(
        &mut self,
        msg: DeleteVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to delete VM via scheduler")
    }
}

impl Message<GetConsoleHistory> for HTTPActor {
    type Reply = Result<GetConsoleHistoryReply, Report>;

    async fn handle(
        &mut self,
        msg: GetConsoleHistory,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to retrieve VM console history via scheduler")
    }
}

impl Message<GetWorkloadLogs> for HTTPActor {
    type Reply = Result<GetWorkloadLogsReply, Report>;

    async fn handle(
        &mut self,
        msg: GetWorkloadLogs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to retrieve VM workload logs via scheduler")
    }
}

impl Message<ShutdownVM> for HTTPActor {
    type Reply = Result<ShutdownVMReply, Report>;

    async fn handle(
        &mut self,
        msg: ShutdownVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to shut down VM via scheduler")
    }
}

impl Message<GetVMInfo> for HTTPActor {
    type Reply = Result<GetVMInfoReply, Report>;

    async fn handle(
        &mut self,
        msg: GetVMInfo,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to get VM info via scheduler")
    }
}

impl Message<AgentListVMs> for HTTPActor {
    type Reply = Result<AgentListVMsReply, Report>;

    async fn handle(
        &mut self,
        msg: AgentListVMs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to list VMs via scheduler")
    }
}

impl Message<CollectRepositoryGarbage> for HTTPActor {
    type Reply = Result<RepositoryGcReply, Report>;

    async fn handle(
        &mut self,
        msg: CollectRepositoryGarbage,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to collect repository garbage via scheduler")
    }
}

impl Message<CreateNetwork> for HTTPActor {
    type Reply = Result<CreateNetworkReply, Report>;

    async fn handle(
        &mut self,
        msg: CreateNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to create network via scheduler")
    }
}

impl Message<DeleteNetwork> for HTTPActor {
    type Reply = Result<DeleteNetworkReply, Report>;

    async fn handle(
        &mut self,
        msg: DeleteNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to delete network via scheduler")
    }
}

impl Message<GetNetwork> for HTTPActor {
    type Reply = Result<GetNetworkReply, Report>;

    async fn handle(
        &mut self,
        msg: GetNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to get network via scheduler")
    }
}

impl Message<ListNetworks> for HTTPActor {
    type Reply = Result<ListNetworksReply, Report>;

    async fn handle(
        &mut self,
        msg: ListNetworks,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.scheduler
            .ask(msg)
            .await
            .map_err(|err| eyre!(err.to_string()))
            .wrap_err("failed to list networks via scheduler")
    }
}
