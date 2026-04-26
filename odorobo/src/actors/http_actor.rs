use cloud_hypervisor_client::models::{CpusConfig, MemoryConfig, PayloadConfig, VmConfig};
use kameo::prelude::*;
use crate::messages::vm::{
        AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
        ShutdownVM, ShutdownVMReply,
    };
use stable_eyre::{Report, Result};

use crate::http_api::types::CreateVMRequest;

use super::scheduler_actor::SchedulerActor;

const EXTERNAL_HTTP_ADDRESS: &str = "0.0.0.0:3000";
const EXTERNAL_HTTP_URL: &str = "http://localhost:3000"; // TODO: mak
/// HTTP REST API service
#[derive(RemoteActor)]
pub struct HTTPActor {
    pub scheduler: ActorRef<SchedulerActor>,
}

impl HTTPActor {
    pub fn create_vm_message(request: CreateVMRequest) -> CreateVM {
        CreateVM {
            vmid: request.data.id,
            config: VmConfig {
                cpus: Some(CpusConfig {
                    boot_vcpus: request.data.vcpus as i32,
                    max_vcpus: request
                        .data
                        .max_vcpus
                        .map(|v| v as i32)
                        .unwrap_or(request.data.vcpus as i32),
                    ..Default::default()
                }),
                memory: Some(MemoryConfig {
                    size: request.data.memory.as_u64() as i64,
                    ..Default::default()
                }),
                payload: PayloadConfig {
                    kernel: Some(request.data.image),
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }
}

impl Actor for HTTPActor {
    type Args = ActorRef<SchedulerActor>;
    type Error = Report;

    async fn on_start(
        scheduler: Self::Args,
        actor_ref: ActorRef<Self>,
    ) -> Result<Self, Self::Error> {
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

        Ok(Self { scheduler })
    }
}

impl Message<CreateVM> for HTTPActor {
    type Reply = Result<CreateVMReply, Report>;

    async fn handle(
        &mut self,
        msg: CreateVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.scheduler.ask(msg).await?)
    }
}

impl Message<DeleteVM> for HTTPActor {
    type Reply = Result<DeleteVMReply, Report>;

    async fn handle(
        &mut self,
        msg: DeleteVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.scheduler.ask(msg).await?)
    }
}

impl Message<ShutdownVM> for HTTPActor {
    type Reply = Result<ShutdownVMReply, Report>;

    async fn handle(
        &mut self,
        msg: ShutdownVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.scheduler.ask(msg).await?)
    }
}

impl Message<AgentListVMs> for HTTPActor {
    type Reply = Result<AgentListVMsReply, Report>;

    async fn handle(
        &mut self,
        msg: AgentListVMs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.scheduler.ask(msg).await?)
    }
}
