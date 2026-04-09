use std::ops::ControlFlow;

use kameo::prelude::*;
use odorobo_agent::actor::AgentActor;
use odorobo_shared::messages::create_vm::*;
use odorobo_shared::messages::{Ping, Pong};
use stable_eyre::{Report, Result, eyre::eyre};
use tracing::{info, warn};

#[derive(RemoteActor)]
pub struct SchedulerActor {
    pub agent_actor: Option<RemoteActorRef<AgentActor>>,
}

impl SchedulerActor {
    async fn lookup_agent(
        actor_ref: &ActorRef<Self>,
    ) -> Result<RemoteActorRef<AgentActor>, Report> {
        loop {
            let agent_actor_option = RemoteActorRef::<AgentActor>::lookup("agent").await?;

            let Some(agent_actor) = agent_actor_option else {
                warn!("No agent actor currently registered, retrying lookup");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            };

            let agent_actor_peer_id = agent_actor.id().peer_id().unwrap().clone();
            info!("Using agent actor peer id: {agent_actor_peer_id}");

            // remotely link actor, on link death it will be automatically unlinked
            actor_ref.link_remote(&agent_actor).await?;

            return Ok(agent_actor);
        }
    }

    async fn ensure_agent(
        &mut self,
        actor_ref: &ActorRef<Self>,
    ) -> Result<RemoteActorRef<AgentActor>, Report> {
        if let Some(agent_actor) = &self.agent_actor {
            return Ok(agent_actor.clone());
        }

        let new_agent = Self::lookup_agent(actor_ref).await?;
        self.agent_actor = Some(new_agent.clone());
        Ok(new_agent)
    }
}

impl Actor for SchedulerActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let peer_id = actor_ref.id().peer_id().unwrap().clone();

        info!("Actor started! Scheduler peer id: {peer_id}");

        let agent_actor = Self::lookup_agent(&actor_ref).await?;

        Ok(Self {
            agent_actor: Some(agent_actor),
        })
    }

    async fn on_link_died(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        warn!("Linked actor {id:?} died with reason {reason:?}");

        self.agent_actor = None;

        let Some(actor_ref) = actor_ref.upgrade() else {
            return Ok(ControlFlow::Break(ActorStopReason::Killed));
        };

        let new_agent = Self::lookup_agent(&actor_ref).await?;
        self.agent_actor = Some(new_agent);

        Ok(ControlFlow::Continue(()))
    }
}

impl Message<CreateVM> for SchedulerActor {
    type Reply = Result<CreateVMReply, Report>;

    async fn handle(&mut self, msg: CreateVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let actor_ref = ctx.actor_ref();

        let first_agent = self.ensure_agent(&actor_ref).await?;
        match first_agent.ask(&msg).await {
            Ok(reply) => Ok(reply),
            Err(first_err) => {
                warn!(
                    "CreateVM forwarding failed, clearing cached agent and retrying lookup: {first_err}"
                );
                self.agent_actor = None;

                let retry_agent = self.ensure_agent(&actor_ref).await?;
                retry_agent.ask(&msg).await.map_err(|retry_err| {
                    eyre!(
                        "failed to forward CreateVM to agent actor after reconnect; first error: {first_err}; retry error: {retry_err}"
                    )
                })
            }
        }
    }
}

impl Message<DeleteVM> for SchedulerActor {
    type Reply = Result<DeleteVMReply, Report>;

    async fn handle(&mut self, msg: DeleteVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let actor_ref = ctx.actor_ref();

        let first_agent = self.ensure_agent(&actor_ref).await?;
        match first_agent.ask(&msg).await {
            Ok(reply) => Ok(reply),
            Err(first_err) => {
                warn!(
                    "DeleteVM forwarding failed, clearing cached agent and retrying lookup: {first_err}"
                );
                self.agent_actor = None;

                let retry_agent = self.ensure_agent(&actor_ref).await?;
                retry_agent.ask(&msg).await.map_err(|retry_err| {
                    eyre!(
                        "failed to forward DeleteVM to agent actor after reconnect; first error: {first_err}; retry error: {retry_err}"
                    )
                })
            }
        }
    }
}

impl Message<ShutdownVM> for SchedulerActor {
    type Reply = Result<ShutdownVMReply, Report>;

    async fn handle(
        &mut self,
        msg: ShutdownVM,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let actor_ref = ctx.actor_ref();

        let first_agent = self.ensure_agent(&actor_ref).await?;
        match first_agent.ask(&msg).await {
            Ok(reply) => Ok(reply),
            Err(first_err) => {
                warn!(
                    "ShutdownVM forwarding failed, clearing cached agent and retrying lookup: {first_err}"
                );
                self.agent_actor = None;

                let retry_agent = self.ensure_agent(&actor_ref).await?;
                retry_agent.ask(&msg).await.map_err(|retry_err| {
                    eyre!(
                        "failed to forward ShutdownVM to agent actor after reconnect; first error: {first_err}; retry error: {retry_err}"
                    )
                })
            }
        }
    }
}

impl Message<AgentListVMs> for SchedulerActor {
    type Reply = Result<AgentListVMsReply, Report>;

    async fn handle(
        &mut self,
        msg: AgentListVMs,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let actor_ref = ctx.actor_ref();

        let first_agent = self.ensure_agent(&actor_ref).await?;
        match first_agent.ask(&msg).await {
            Ok(reply) => Ok(reply),
            Err(first_err) => {
                warn!(
                    "AgentListVMs forwarding failed, clearing cached agent and retrying lookup: {first_err}"
                );
                self.agent_actor = None;

                let retry_agent = self.ensure_agent(&actor_ref).await?;
                retry_agent.ask(&msg).await.map_err(|retry_err| {
                    eyre!(
                        "failed to forward AgentListVMs to agent actor after reconnect; first error: {first_err}; retry error: {retry_err}"
                    )
                })
            }
        }
    }
}

impl Message<Ping> for SchedulerActor {
    type Reply = Pong;

    async fn handle(&mut self, _msg: Ping, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        Pong
    }
}
