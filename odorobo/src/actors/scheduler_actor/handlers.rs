//! Actor lifecycle implementation and public scheduler message handlers.

use std::ops::ControlFlow;
use std::time::Instant;

use ahash::AHashMap;
use kameo::prelude::*;
use stable_eyre::{Report, eyre::eyre};
use tracing::{info, warn};

use crate::ch_driver::actor::VMActor;
use crate::messages::vm::{
    AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
    GetConsoleHistory, GetConsoleHistoryReply, GetVMInfo, GetVMInfoReply, SendConsoleInput,
    SendConsoleInputReply, ShutdownVM, ShutdownVMReply,
};
use crate::messages::{Ping, Pong};
use crate::utils::actor_names::vm_actor_id;

use super::{CachedActorKind, CachedVMActor, SchedulerActor, VmLifecycle, VmPlacement};

/// Owns scheduler initialization and cleanup for linked remote actors.
///
/// Losing an agent removes its placements. Losing a VM removes only its actor
/// cache entry unless no discovered actor or placeholder remains for that VM.
impl Actor for SchedulerActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let peer_id = *actor_ref.id().peer_id().unwrap();

        info!(?peer_id, "Scheduler Actor started!");

        let mut scheduler_actor = Self {
            agent_data_cache: AHashMap::new(),
            agent_keepalive_tasks: AHashMap::new(),
            vm_actorid_ulid_map: AHashMap::new(),
            vm_manifests: AHashMap::new(),
            stopped_vms: Default::default(),
            retired_vm_actors: Default::default(),
            vm_placements: AHashMap::new(),
            vm_data_cache: AHashMap::new(),
            vm_keepalive_tasks: AHashMap::new(),
            pending_resources_cache: None,
            agent_vm_index: AHashMap::new(),
            actor_kinds: AHashMap::new(),
            cache_actor_finder: None,
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
/// Optimistically reserves a placement, then forwards VM creation to the chosen agent.
///
/// A successful reply confirms agent acceptance, not observed VM execution. A failed
/// request is rolled back only when discovery cannot find a VM actor, preserving state
/// for eventual reconciliation when the request result was lost or delayed.
impl Message<CreateVM> for SchedulerActor {
    type Reply = Result<CreateVMReply, Report>;

    async fn handle(
        &mut self,
        msg: CreateVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if self
            .vm_manifests
            .get(&msg.vmid)
            .is_some_and(|existing| existing != &msg.config)
        {
            return Err(eyre!("conflicting create request for existing VM ID"));
        }
        // Cold stop can remove scheduler intent while retaining the OCI owner.
        // Discover ownership before scheduling: local persistent state must not
        // silently move to a second agent with a fresh upper.
        if let Some(owner) = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await? {
            let info = owner
                .ask(&GetVMInfo {
                    vmid: Some(msg.vmid),
                })
                .await?;
            if info.config.as_ref() != Some(&msg.config) {
                return Err(eyre!("conflicting create request for retained VM owner"));
            }
            owner.ask(&crate::ch_driver::actor::StartVM).await?;
            self.stopped_vms.remove(&msg.vmid);
            self.vm_manifests.insert(msg.vmid, msg.config.clone());
            self.vm_actorid_ulid_map.insert(owner.id(), msg.vmid);
            Self::reconcile_discovered_vm(
                msg.vmid,
                &self.agent_vm_index,
                &self.vm_manifests,
                &mut self.vm_placements,
            );
            Self::update_cached_vm_entry(
                self.vm_data_cache.entry(msg.vmid).or_default(),
                owner.id(),
                CachedVMActor {
                    actor_ref: Some(owner.clone()),
                },
            );
            self.invalidate_pending_resources();
            return Ok(CreateVMReply {
                config: Some(msg.config),
                actor_id: Some(owner.id().to_bytes()),
            });
        }
        if self.vm_manifests.contains_key(&msg.vmid) {
            return Err(eyre!(
                "VM intent exists but owner is unavailable; retry discovery"
            ));
        }

        let target_agent = self.schedule_agent(&msg)?;

        self.stopped_vms.remove(&msg.vmid);
        self.vm_manifests.insert(msg.vmid, msg.config.clone());
        self.invalidate_pending_resources();
        self.vm_placements
            .entry(msg.vmid)
            .or_default()
            .push(VmPlacement {
                agent_id: target_agent.id(),
                lifecycle: VmLifecycle::Pending,
                created_at: Instant::now(),
                last_confirmed_at: None,
            });
        self.vm_data_cache
            .entry(msg.vmid)
            .or_default()
            .push(CachedVMActor { actor_ref: None });

        let reply = target_agent.ask(&msg).await;

        if let Ok(reply) = &reply
            && let Some(actor_id_bytes) = &reply.actor_id
            && let Ok(actor_id) = ActorId::from_bytes(actor_id_bytes)
        {
            self.vm_actorid_ulid_map.insert(actor_id, msg.vmid);
        }

        if reply.is_err() {
            let actor_exists = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid))
                .await
                .ok()
                .flatten()
                .is_some();
            Self::rollback_failed_create(
                msg.vmid,
                actor_exists,
                reply.as_ref().ok().and_then(|reply| {
                    reply
                        .actor_id
                        .as_deref()
                        .and_then(|bytes| ActorId::from_bytes(bytes).ok())
                }),
                &mut self.vm_actorid_ulid_map,
                &mut self.vm_manifests,
                &mut self.vm_placements,
                &mut self.vm_data_cache,
            );
        }

        Ok(reply?)
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

/// Looks up a VM actor, forwards deletion, and removes desired intent.
impl Message<DeleteVM> for SchedulerActor {
    type Reply = Result<DeleteVMReply, Report>;

    async fn handle(
        &mut self,
        msg: DeleteVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?;
        tracing::trace!(?vm, "DeleteVM");
        if let Some(vm) = vm {
            let reply: DeleteVMReply = vm.ask(&msg).await?;
            if let Some(error) = &reply.error {
                return Err(eyre!("VM {} deletion failed: {error}", msg.vmid));
            }
            // Tombstone successful deletion just like shutdown: queued
            // pre-delete discovery must not recreate removed persistent state.
            self.retired_vm_actors.insert(vm.id());
            self.suppress_vm_intent(msg.vmid);
            Ok(reply)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

/// Looks up a VM actor, forwards shutdown, and suppresses automatic recreation.
impl Message<ShutdownVM> for SchedulerActor {
    type Reply = Result<ShutdownVMReply, Report>;

    async fn handle(
        &mut self,
        msg: ShutdownVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?;
        tracing::trace!(?vm, "ShutdownVM");
        if let Some(vm) = vm {
            vm.ask(&msg).await?;
            self.suppress_vm_intent(msg.vmid);
            Ok(ShutdownVMReply)
        } else {
            Err(eyre!("VM not found"))
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
