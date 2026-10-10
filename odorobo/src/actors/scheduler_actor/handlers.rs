//! Actor lifecycle implementation and public scheduler message handlers.

use std::{
    ops::ControlFlow,
    sync::Arc,
    time::{Duration, Instant},
};

use ahash::AHashMap;
use kameo::prelude::*;
use stable_eyre::{Report, eyre::eyre};
use tracing::{info, warn};

use crate::ch_driver::actor::VMActor;
use crate::cluster_state::{
    PlacementLifecycle, PlacementRecord, StateError, StateStore, StopIntent,
};
use crate::messages::vm::{
    AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
    GetConsoleHistory, GetConsoleHistoryReply, GetVMInfo, GetVMInfoReply, SendConsoleInput,
    SendConsoleInputReply, ShutdownVM, ShutdownVMReply,
};
use crate::messages::{Ping, Pong};
use crate::utils::actor_names::vm_actor_id;
use ulid::Ulid;

use super::{CachedActorKind, CachedVMActor, SchedulerActor, VmLifecycle, VmPlacement};

impl SchedulerActor {
    async fn refresh_durable_state(&mut self) -> Result<(), Report> {
        let states = self
            .state_store
            .list_vm_state::<crate::manifest::VmManifest>()
            .await
            .map_err(|error| eyre!("unable to refresh durable VM state: {error}"))?;
        if states
            .iter()
            .any(|state| state.manifest.id != state.placement.vmid)
        {
            return Err(eyre!(
                "durable manifest ID does not match its placement key"
            ));
        }
        self.vm_manifests = states
            .iter()
            .map(|state| (state.manifest.id, state.manifest.clone()))
            .collect();
        self.durable_placements = states
            .into_iter()
            .map(|state| (state.placement.vmid, state.placement))
            .collect();
        Ok(())
    }

    fn owner_agent(
        &self,
        node: &str,
    ) -> Result<RemoteActorRef<crate::actors::agent_actor::AgentActor>, Report> {
        let mut owners = self
            .agent_data_cache
            .values()
            .filter(|agent| agent.data.hostname == node)
            .map(|agent| agent.actor_ref.clone());
        let owner = owners
            .next()
            .ok_or_else(|| eyre!("VM owner {node} is unavailable; durable state was retained"))?;
        if owners.next().is_some() {
            return Err(eyre!("VM owner hostname {node} is ambiguous"));
        }
        Ok(owner)
    }
}

/// Owns scheduler initialization and cleanup for linked remote actors.
///
/// Losing an agent removes its placements. Losing a VM removes only its actor
/// cache entry unless no discovered actor or placeholder remains for that VM.
impl Actor for SchedulerActor {
    type Args = Arc<StateStore>;
    type Error = Report;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let peer_id = *actor_ref.id().peer_id().unwrap();

        info!(?peer_id, "Scheduler Actor started!");

        let states = args
            .list_vm_state::<crate::manifest::VmManifest>()
            .await
            .map_err(|error| eyre!("unable to load durable VM state: {error}"))?;
        if states
            .iter()
            .any(|state| state.manifest.id != state.placement.vmid)
        {
            return Err(eyre!(
                "durable manifest ID does not match its placement key"
            ));
        }
        let vm_manifests = states
            .iter()
            .map(|state| (state.manifest.id, state.manifest.clone()))
            .collect();
        let durable_placements = states
            .into_iter()
            .map(|state| (state.placement.vmid, state.placement))
            .collect();

        let mut scheduler_actor = Self {
            agent_data_cache: AHashMap::new(),
            agent_keepalive_tasks: AHashMap::new(),
            vm_actorid_ulid_map: AHashMap::new(),
            vm_manifests,
            vm_placements: AHashMap::new(),
            durable_placements,
            vm_data_cache: AHashMap::new(),
            vm_keepalive_tasks: AHashMap::new(),
            pending_resources_cache: None,
            agent_vm_index: AHashMap::new(),
            actor_kinds: AHashMap::new(),
            cache_actor_finder: None,
            state_store: args,
        };

        scheduler_actor.start_actor_finder(actor_ref);

        Ok(scheduler_actor)
    }

    async fn on_link_died(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        warn!(?id, ?reason, "Linked actor died");

        // check that scheduler actor is still alive.
        let Some(_) = actor_ref.upgrade() else {
            return Ok(ControlFlow::Break(ActorStopReason::Killed));
        };

        match self.actor_kinds.remove(&id) {
            Some(CachedActorKind::Agent) => self.cleanup_agent_actor(id),
            Some(CachedActorKind::Vm) => self.cleanup_vm_actor(id),
            None => {}
        }

        Ok(ControlFlow::Continue(()))
    }
}
/// Persists an authoritative placement before an explicit create dispatch.
impl Message<CreateVM> for SchedulerActor {
    type Reply = Result<CreateVMReply, Report>;

    async fn handle(
        &mut self,
        msg: CreateVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.refresh_durable_state().await?;
        if msg.config.id != msg.vmid {
            return Err(eyre!("manifest ID does not match requested VM ID"));
        }
        let (placement, target_agent) =
            if let Some(existing) = self.durable_placements.get(&msg.vmid) {
                if existing.lifecycle != PlacementLifecycle::Active
                    || self.vm_manifests.get(&msg.vmid) != Some(&msg.config)
                {
                    return Err(eyre!("conflicting or stopping VM intent"));
                }
                (existing.clone(), self.owner_agent(&existing.node)?)
            } else {
                let target_agent = self.schedule_agent(&msg)?;
                let target_id = target_agent.id();
                let node = self
                    .agent_data_cache
                    .get(&target_id)
                    .map(|agent| agent.data.hostname.clone())
                    .filter(|hostname| !hostname.is_empty())
                    .ok_or_else(|| eyre!("selected agent has no registered hostname"))?;
                if self.owner_agent(&node)?.id() != target_id {
                    return Err(eyre!(
                        "selected agent hostname does not uniquely identify its owner"
                    ));
                }
                (
                    PlacementRecord {
                        vmid: msg.vmid,
                        node,
                        generation: Some(Ulid::generate()),
                        lifecycle: PlacementLifecycle::Active,
                    },
                    target_agent,
                )
            };

        self.state_store
            .create_vm_state(msg.vmid, &msg.config, &placement)
            .await
            .map_err(|error| eyre!("unable to persist VM state: {error}"))?;
        let mut request = msg.clone();
        request.placement = Some(placement.clone());
        self.vm_manifests.insert(msg.vmid, msg.config.clone());
        self.durable_placements.insert(msg.vmid, placement);
        let target_agent_id = target_agent.id();
        if !self.vm_placements.get(&msg.vmid).is_some_and(|placements| {
            placements
                .iter()
                .any(|entry| entry.agent_id == target_agent_id)
        }) {
            self.vm_placements
                .entry(msg.vmid)
                .or_default()
                .push(VmPlacement {
                    agent_id: target_agent_id,
                    lifecycle: VmLifecycle::Pending,
                    created_at: Instant::now(),
                    last_confirmed_at: None,
                });
            self.vm_data_cache
                .entry(msg.vmid)
                .or_default()
                .push(CachedVMActor { actor_ref: None });
            self.invalidate_pending_resources();
        }

        let reply = tokio::time::timeout(Duration::from_secs(30), target_agent.ask(&request))
            .await
            .map_err(|_| {
                eyre!(
                    "timed out creating VM {}; durable intent retained",
                    msg.vmid
                )
            })??;
        if let Some(error) = &reply.error {
            return Err(eyre!("agent rejected VM create: {error}"));
        }
        let actor_id_bytes = reply.actor_id.as_deref().ok_or_else(|| {
            eyre!(
                "agent did not confirm VM {}; durable intent retained",
                msg.vmid
            )
        })?;
        let actor_id = ActorId::from_bytes(actor_id_bytes)
            .map_err(|error| eyre!("agent returned an invalid VM actor ID: {error}"))?;
        self.vm_actorid_ulid_map.insert(actor_id, msg.vmid);
        Ok(reply)
    }
}

impl Message<GetConsoleHistory> for SchedulerActor {
    type Reply = Result<GetConsoleHistoryReply, Report>;

    async fn handle(
        &mut self,
        msg: GetConsoleHistory,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?;
        tracing::trace!(?vm, vmid = %msg.vmid, "GetConsoleHistory");
        if let Some(vm) = vm {
            Ok(vm.ask(&msg).await?)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

impl Message<SendConsoleInput> for SchedulerActor {
    type Reply = Result<SendConsoleInputReply, Report>;

    async fn handle(
        &mut self,
        msg: SendConsoleInput,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?;
        tracing::trace!(
            ?vm,
            vmid = %msg.vmid,
            bytes = msg.input.len(),
            "SendConsoleInput"
        );
        if let Some(vm) = vm {
            Ok(vm.ask(&msg).await?)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

/// Persists stop intent, dispatches only to the recorded owner, then CAS-deletes state.
impl Message<DeleteVM> for SchedulerActor {
    type Reply = Result<DeleteVMReply, Report>;

    async fn handle(
        &mut self,
        mut msg: DeleteVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.refresh_durable_state().await?;
        let expected = match self
            .state_store
            .begin_vm_stop(msg.vmid, StopIntent::Delete)
            .await
        {
            Ok(expected) => expected,
            // A prior confirmed delete may have committed but lost its reply.
            // The refresh above rejects incomplete pairs before this case.
            Err(StateError::Missing) => {
                self.remove_vm_intent(msg.vmid);
                return Ok(DeleteVMReply { error: None });
            }
            Err(error) => return Err(eyre!("unable to mark VM deleting: {error}")),
        };
        self.durable_placements.insert(msg.vmid, expected.clone());
        let owner = self.owner_agent(&expected.node)?;
        msg.placement = Some(expected.clone());
        let reply = tokio::time::timeout(Duration::from_secs(30), owner.ask(&msg))
            .await
            .map_err(|_| eyre!("timed out deleting VM {}; stop marker retained", msg.vmid))??;
        drop(owner);
        if let Some(error) = reply.error {
            return Err(eyre!("VM teardown unconfirmed: {error}"));
        }
        self.state_store
            .complete_vm_stop(&expected)
            .await
            .map_err(|error| eyre!("unable to finalize VM delete: {error}"))?;
        self.remove_vm_intent(msg.vmid);
        Ok(DeleteVMReply { error: None })
    }
}

impl Message<ShutdownVM> for SchedulerActor {
    type Reply = Result<ShutdownVMReply, Report>;

    async fn handle(
        &mut self,
        mut msg: ShutdownVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.refresh_durable_state().await?;
        let expected = match self
            .state_store
            .begin_vm_stop(msg.vmid, StopIntent::Shutdown)
            .await
        {
            Ok(expected) => expected,
            // As with delete, both absent records mean a prior finalization is
            // already complete; refresh_durable_state rejects incomplete pairs.
            Err(StateError::Missing) => {
                self.remove_vm_intent(msg.vmid);
                return Ok(ShutdownVMReply { error: None });
            }
            Err(error) => return Err(eyre!("unable to mark VM stopping: {error}")),
        };
        self.durable_placements.insert(msg.vmid, expected.clone());
        let owner = self.owner_agent(&expected.node)?;
        msg.placement = Some(expected.clone());
        tokio::time::timeout(Duration::from_secs(30), owner.ask(&msg))
            .await
            .map_err(|_| {
                eyre!(
                    "timed out shutting down VM {}; stop marker retained",
                    msg.vmid
                )
            })?
            .map_err(|error| eyre!("VM teardown unconfirmed: {error}"))?;
        self.state_store
            .complete_vm_stop(&expected)
            .await
            .map_err(|error| eyre!("unable to finalize VM shutdown: {error}"))?;
        self.remove_vm_intent(msg.vmid);
        Ok(ShutdownVMReply { error: None })
    }
}

/// Looks up a VM actor and forwards an info request.
impl Message<GetVMInfo> for SchedulerActor {
    type Reply = Result<GetVMInfoReply, Report>;

    async fn handle(
        &mut self,
        msg: GetVMInfo,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vmid = msg.vmid.ok_or_else(|| eyre!("VM ID is required"))?;
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(vmid)).await?;

        let Some(vm) = vm else {
            return Ok(GetVMInfoReply { vmid, config: None });
        };

        Ok(vm.ask(&msg).await?)
    }
}

/// Returns the concatenated VM IDs from cached agent status snapshots.
///
/// This is a potentially stale, non-deduplicated observation rather than an
/// authoritative inventory; it performs neither polling nor direct VM-actor queries.
impl Message<AgentListVMs> for SchedulerActor {
    type Reply = Result<AgentListVMsReply, Report>;

    async fn handle(
        &mut self,
        _msg: AgentListVMs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let total_vms = self
            .agent_data_cache
            .values()
            .map(|agent| agent.data.vms.len())
            .sum();
        let mut vms = Vec::with_capacity(total_vms);

        for agent in self.agent_data_cache.values() {
            vms.extend_from_slice(agent.data.vms.as_slice());
        }

        Ok(AgentListVMsReply { vms })
    }
}

/// Provides scheduler actor liveness only; it does not imply cache freshness or readiness.
impl Message<Ping> for SchedulerActor {
    type Reply = Pong;

    async fn handle(&mut self, _msg: Ping, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        Pong
    }
}
