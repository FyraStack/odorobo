//! Actor lifecycle implementation and public scheduler message handlers.

use std::{ops::ControlFlow, sync::Arc, time::Instant};

use ahash::AHashMap;
use kameo::prelude::*;
use stable_eyre::{Report, eyre::eyre};
use tracing::{info, warn};

use crate::actors::agent_actor::AgentActor;
use crate::ch_driver::actor::VMActor;
use crate::cluster_state::{
    PlacementLifecycle, PlacementRecord, StateError, StateStore, StopIntent, VmState,
    VmStateSnapshot,
};
use crate::manifest::VmManifest;
use crate::messages::vm::{
    AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
    GetConsoleHistory, GetConsoleHistoryReply, GetVMInfo, GetVMInfoReply, SendConsoleInput,
    SendConsoleInputReply, ShutdownVM, ShutdownVMReply, VM_CREATE_REQUEST_TIMEOUT,
    VM_STOP_REQUEST_TIMEOUT,
};
use crate::messages::{Ping, Pong};
use crate::utils::actor_names::vm_actor_id;
use ulid::Ulid;

use super::{CachedActorKind, CachedVMActor, SchedulerActor, VmLifecycle, VmPlacement};

/// Delivered by a delegated create once the owner confirms the VM actor.
struct VmCreateConfirmed {
    vmid: Ulid,
    actor_id: ActorId,
}

/// Delivered by a delegated delete/shutdown once durable state is finalized.
struct VmStopFinalized {
    placement: PlacementRecord,
}

impl SchedulerActor {
    /// Splits a durable snapshot into loadable state, logging each VM that is
    /// skipped so one damaged pair cannot block the manager or other VMs.
    fn load_durable_state(
        snapshot: VmStateSnapshot<VmManifest>,
    ) -> (AHashMap<Ulid, VmManifest>, AHashMap<Ulid, PlacementRecord>) {
        for invalid in &snapshot.invalid {
            warn!(
                vm = %invalid.id,
                error = %invalid.error,
                "Skipping invalid durable VM state; operator investigation required"
            );
        }
        let mut manifests = AHashMap::new();
        let mut placements = AHashMap::new();
        for state in snapshot.states {
            let vmid = state.placement.vmid;
            if state.manifest.id != vmid {
                warn!(%vmid, manifest_id = %state.manifest.id, "Skipping durable VM whose manifest ID does not match its key");
                continue;
            }
            manifests.insert(vmid, state.manifest);
            placements.insert(vmid, state.placement);
        }
        (manifests, placements)
    }

    /// Re-reads one VM's durable state and mirrors it into the local caches.
    async fn refresh_vm_state(
        &mut self,
        vmid: Ulid,
    ) -> Result<Option<VmState<VmManifest>>, Report> {
        let state = self
            .state_store
            .get_vm_state::<VmManifest>(vmid)
            .await
            .map_err(|error| eyre!("unable to read durable state for VM {vmid}: {error}"))?;
        match &state {
            Some(state) if state.manifest.id != vmid => {
                return Err(eyre!(
                    "durable manifest ID for VM {vmid} does not match its key"
                ));
            }
            Some(state) => {
                self.vm_manifests.insert(vmid, state.manifest.clone());
                self.durable_placements
                    .insert(vmid, state.placement.clone());
            }
            None => {
                self.durable_placements.remove(&vmid);
            }
        }
        Ok(state)
    }

    /// Persists a stop marker and resolves the recorded owner. `None` means
    /// both records are already absent (a prior stop finalized).
    async fn begin_stop(
        &mut self,
        vmid: Ulid,
        intent: StopIntent,
    ) -> Result<Option<(PlacementRecord, RemoteActorRef<AgentActor>)>, Report> {
        let expected = match self.state_store.begin_vm_stop(vmid, intent).await {
            Ok(expected) => expected,
            // A prior confirmed stop may have committed but lost its reply.
            Err(StateError::Missing) => {
                self.remove_vm_intent(vmid);
                return Ok(None);
            }
            Err(error) => {
                return Err(eyre!("unable to mark VM {vmid} for {intent:?}: {error}"));
            }
        };
        self.durable_placements.insert(vmid, expected.clone());
        let owner = self.owner_agent(&expected.node)?;
        Ok(Some((expected, owner)))
    }

    fn owner_agent(&self, node: &str) -> Result<RemoteActorRef<AgentActor>, Report> {
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

        let snapshot = args
            .list_vm_state::<VmManifest>()
            .await
            .map_err(|error| eyre!("unable to load durable VM state: {error}"))?;
        let (vm_manifests, durable_placements) = Self::load_durable_state(snapshot);

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
///
/// The owner's reply is awaited off the scheduler's message loop, so a slow VM
/// start does not stall discovery and other requests.
impl Message<CreateVM> for SchedulerActor {
    type Reply = DelegatedReply<Result<CreateVMReply, Report>>;

    async fn handle(&mut self, msg: CreateVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        match self.prepare_create(&msg).await {
            Ok((request, target_agent)) => {
                let scheduler = ctx.actor_ref().clone();
                ctx.spawn(async move {
                    let vmid = request.vmid;
                    let reply =
                        tokio::time::timeout(VM_CREATE_REQUEST_TIMEOUT, target_agent.ask(&request))
                            .await
                            .map_err(|_| {
                                eyre!("timed out creating VM {vmid}; durable intent retained")
                            })??;
                    if let Some(error) = &reply.error {
                        return Err(eyre!("agent rejected VM create: {error}"));
                    }
                    let actor_id_bytes = reply.actor_id.as_deref().ok_or_else(|| {
                        eyre!("agent did not confirm VM {vmid}; durable intent retained")
                    })?;
                    let actor_id = ActorId::from_bytes(actor_id_bytes)
                        .map_err(|error| eyre!("agent returned an invalid VM actor ID: {error}"))?;
                    if let Err(error) = scheduler.tell(VmCreateConfirmed { vmid, actor_id }).await {
                        warn!(%vmid, ?error, "Unable to record confirmed VM actor");
                    }
                    Ok(reply)
                })
            }
            Err(error) => ctx.reply(Err(error)),
        }
    }
}

impl SchedulerActor {
    /// Validates or persists the durable placement for a create and reserves
    /// its pending placement, returning the request to dispatch and its owner.
    async fn prepare_create(
        &mut self,
        msg: &CreateVM,
    ) -> Result<(CreateVM, RemoteActorRef<AgentActor>), Report> {
        if msg.config.id != msg.vmid {
            return Err(eyre!("manifest ID does not match requested VM ID"));
        }
        let existing = self.refresh_vm_state(msg.vmid).await?;
        let (placement, target_agent) = if let Some(existing) = existing {
            if existing.placement.lifecycle != PlacementLifecycle::Active
                || existing.manifest != msg.config
            {
                return Err(eyre!("conflicting or stopping VM intent"));
            }
            let owner = self.owner_agent(&existing.placement.node)?;
            (existing.placement, owner)
        } else {
            let target_agent = self.schedule_agent(msg)?;
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
        Ok((request, target_agent))
    }
}

impl Message<VmCreateConfirmed> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: VmCreateConfirmed, _ctx: &mut Context<Self, Self::Reply>) {
        // Ignore a confirmation that raced with a stop of the same VM.
        if self.durable_placements.contains_key(&msg.vmid) {
            self.vm_actorid_ulid_map.insert(msg.actor_id, msg.vmid);
        }
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
///
/// The marker is written on the message loop; the owner's teardown and the
/// final compare-and-delete run off it so the scheduler stays responsive.
impl Message<DeleteVM> for SchedulerActor {
    type Reply = DelegatedReply<Result<DeleteVMReply, Report>>;

    async fn handle(
        &mut self,
        mut msg: DeleteVM,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let (expected, owner) = match self.begin_stop(msg.vmid, StopIntent::Delete).await {
            Ok(Some(stop)) => stop,
            Ok(None) => return ctx.reply(Ok(DeleteVMReply { error: None })),
            Err(error) => return ctx.reply(Err(error)),
        };
        msg.placement = Some(expected.clone());
        let store = Arc::clone(&self.state_store);
        let scheduler = ctx.actor_ref().clone();
        ctx.spawn(async move {
            let vmid = msg.vmid;
            let reply = tokio::time::timeout(VM_STOP_REQUEST_TIMEOUT, owner.ask(&msg))
                .await
                .map_err(|_| eyre!("timed out deleting VM {vmid}; stop marker retained"))??;
            finalize_stop(&store, &scheduler, &expected, reply.error).await?;
            Ok(DeleteVMReply { error: None })
        })
    }
}

/// Same protocol as [`DeleteVM`]; durable state is removed once teardown is confirmed.
impl Message<ShutdownVM> for SchedulerActor {
    type Reply = DelegatedReply<Result<ShutdownVMReply, Report>>;

    async fn handle(
        &mut self,
        mut msg: ShutdownVM,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let (expected, owner) = match self.begin_stop(msg.vmid, StopIntent::Shutdown).await {
            Ok(Some(stop)) => stop,
            Ok(None) => return ctx.reply(Ok(ShutdownVMReply { error: None })),
            Err(error) => return ctx.reply(Err(error)),
        };
        msg.placement = Some(expected.clone());
        let store = Arc::clone(&self.state_store);
        let scheduler = ctx.actor_ref().clone();
        ctx.spawn(async move {
            let vmid = msg.vmid;
            let reply = tokio::time::timeout(VM_STOP_REQUEST_TIMEOUT, owner.ask(&msg))
                .await
                .map_err(|_| eyre!("timed out shutting down VM {vmid}; stop marker retained"))??;
            finalize_stop(&store, &scheduler, &expected, reply.error).await?;
            Ok(ShutdownVMReply { error: None })
        })
    }
}

/// Removes durable state only after the owner confirmed teardown.
async fn finalize_stop(
    store: &StateStore,
    scheduler: &ActorRef<SchedulerActor>,
    expected: &PlacementRecord,
    teardown_error: Option<String>,
) -> Result<(), Report> {
    let vmid = expected.vmid;
    if let Some(error) = teardown_error {
        return Err(eyre!(
            "VM {vmid} teardown unconfirmed: {error}; stop marker retained"
        ));
    }
    store
        .complete_vm_stop(expected)
        .await
        .map_err(|error| eyre!("unable to finalize stop of VM {vmid}: {error}"))?;
    let finalized = VmStopFinalized {
        placement: expected.clone(),
    };
    if let Err(error) = scheduler.tell(finalized).await {
        warn!(%vmid, ?error, "Unable to clear finalized VM from scheduler cache");
    }
    Ok(())
}

impl Message<VmStopFinalized> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: VmStopFinalized, _ctx: &mut Context<Self, Self::Reply>) {
        let vmid = msg.placement.vmid;
        // A create may have re-used this VM ID after finalization; keep it.
        if self
            .durable_placements
            .get(&vmid)
            .is_none_or(|current| current.generation == msg.placement.generation)
        {
            self.remove_vm_intent(vmid);
        }
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
