use kameo::prelude::*;
use stable_eyre::{Report, Result};

use super::scheduler_actor::SchedulerActor;

pub const EXTERNAL_HTTP_ADDRESS: &str = "0.0.0.0:3000";

#[derive(RemoteActor)]
pub struct HTTPActor {
    pub scheduler: ActorRef<SchedulerActor>,
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
            axum::serve(
                listener,
                crate::http_api::build(actor_ref.into_remote_ref().await),
            )
            .await
            .unwrap();
        });

        Ok(Self { scheduler })
    }
}

macro_rules! impl_msg {
    (@$type:ty) => {
        ::preinterpret::preinterpret! {
            use crate::messages::vm::{$type, [!ident! $type Reply], [!ident! $type Error]};
            #[remote_message]
            impl Message<$type> for HTTPActor {
                type Reply = Result<[!ident! $type Reply], [!ident! $type Error]>;

                async fn handle(
                    &mut self,
                    msg: $type,
                    _ctx: &mut Context<Self, Self::Reply>,
                ) -> Self::Reply {
                    self.scheduler.ask(msg).await.map_err(|e| [!ident! $type Error](e.to_string()))
                }
            }
        }
    };
    ($($types:ident),+$(,)?) => {
        $(impl_msg![@$types];)+
    };
}

impl_msg![CreateVM, DeleteVM, ShutdownVM, AgentListVMs];
