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
use crate::messages::vm::{
    AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
    GetConsoleHistory, GetConsoleHistoryReply, GetVMInfo, GetVMInfoReply, SendConsoleInput,
    SendConsoleInputReply, ShutdownVM, ShutdownVMReply,
};
use crate::messages::{Ping, Pong};
use crate::utils::actor_names::vm_actor_id;
use odorobo::cluster_state::{
    ClusterStateStore, PLACEMENT_PREFIX, PlacementRecord, StateStore, VM_MANIFESTS_PREFIX,
};

use super::{CachedActorKind, CachedVMActor, SchedulerActor, VmLifecycle, VmPlacement};

async fn persist_create_state(
    state_store: &StateStore,
    msg: &CreateVM,
    placement: &PlacementRecord,
) -> Result<(), Report> {
    state_store
        .create_vm_state(msg.vmid, &msg.config, placement)
        .await
        .map_err(|error| eyre!("unable to persist VM state: {error}"))
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

        let vm_manifests = args
            .list::<crate::manifest::VmManifest>(VM_MANIFESTS_PREFIX)
            .await
            .map_err(|error| eyre!("unable to load durable VM manifests: {error}"))?
            .into_iter()
            .map(|(_, manifest)| (manifest.id, manifest))
            .collect();
        let durable_placements = args
            .list::<PlacementRecord>(PLACEMENT_PREFIX)
            .await
            .map_err(|error| eyre!("unable to load durable VM placements: {error}"))?
            .into_iter()
            .map(|(_, placement)| (placement.vmid, placement))
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
/// Optimistically reserves a placement, then forwards VM creation to the chosen agent.
///
/// A successful reply confirms agent acceptance, not observed VM execution. Failed or
/// ambiguous requests retain durable intent so reconciliation can safely retry them.
impl Message<CreateVM> for SchedulerActor {
    type Reply = Result<CreateVMReply, Report>;

    async fn handle(
        &mut self,
        msg: CreateVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Some(existing) = self.vm_manifests.get(&msg.vmid) {
            if existing != &msg.config {
                return Err(eyre!("conflicting create request for existing VM ID"));
            }

            let actor_id = self
                .vm_actorid_ulid_map
                .iter()
                .find_map(|(actor_id, vmid)| (*vmid == msg.vmid).then(|| actor_id.to_bytes()));
            return Ok(CreateVMReply {
                config: Some(existing.clone()),
                actor_id,
            });
        }

        let target_agent = self.schedule_agent(&msg)?;
        let target_agent_id = target_agent.id();

        let node = self
            .agent_data_cache
            .get(&target_agent_id)
            .map_or_else(|| "unknown".to_owned(), |agent| agent.data.hostname.clone());
        let placement = PlacementRecord {
            vmid: msg.vmid,
            node,
        };

        // Record the desired state before creating anything. This prevents a
        // manager crash after agent creation from leaving an unplaced manifest.
        persist_create_state(&self.state_store, &msg, &placement).await?;

        self.vm_manifests.insert(msg.vmid, msg.config.clone());
        self.durable_placements.insert(msg.vmid, placement);
        self.invalidate_pending_resources();
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

        let reply = tokio::time::timeout(Duration::from_secs(30), target_agent.ask(&msg))
            .await
            .map_err(|_| eyre!("timed out creating VM {}", msg.vmid))?;

        if let Ok(reply) = &reply {
            let actor_id_bytes = reply
                .actor_id
                .as_deref()
                .ok_or_else(|| eyre!("agent did not create VM {}", msg.vmid))?;
            let actor_id = ActorId::from_bytes(actor_id_bytes)
                .map_err(|error| eyre!("agent returned an invalid VM actor ID: {error}"))?;
            self.vm_actorid_ulid_map.insert(actor_id, msg.vmid);
        } else {
            warn!(vm_id = %msg.vmid, "VM create result is unknown; retaining durable state for recovery");
        }

        drop(target_agent);
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
        let owner = self
            .durable_placements
            .get(&msg.vmid)
            .and_then(|placement| {
                self.agent_data_cache
                    .values()
                    .find(|agent| agent.data.hostname == placement.node)
                    .map(|agent| agent.actor_ref.clone())
            });
        let vm = if owner.is_none() {
            RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?
        } else {
            None
        };
        tracing::trace!(?vm, "DeleteVM");
        let reply = if let Some(owner) = owner.as_ref() {
            tokio::time::timeout(Duration::from_secs(30), owner.ask(&msg))
                .await
                .map_err(|_| eyre!("timed out deleting VM {}", msg.vmid))??
        } else if let Some(vm) = vm {
            tokio::time::timeout(Duration::from_secs(30), vm.ask(&msg))
                .await
                .map_err(|_| eyre!("timed out deleting VM {}", msg.vmid))??
        } else if self.durable_placements.contains_key(&msg.vmid) {
            return Err(eyre!("VM owner is unavailable; durable state was retained"));
        } else {
            DeleteVMReply { error: None }
        };
        drop(owner);
        if let Some(error) = reply.error {
            return Err(eyre!("unable to delete VM: {error}"));
        }
        self.state_store
            .delete_vm_state(msg.vmid)
            .await
            .map_err(|error| eyre!("unable to delete durable VM state: {error}"))?;
        self.remove_vm_intent(msg.vmid);
        Ok(DeleteVMReply { error: None })
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
        let owner = self
            .durable_placements
            .get(&msg.vmid)
            .and_then(|placement| {
                self.agent_data_cache
                    .values()
                    .find(|agent| agent.data.hostname == placement.node)
                    .map(|agent| agent.actor_ref.clone())
            });
        let vm = if owner.is_none() {
            RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?
        } else {
            None
        };
        tracing::trace!(?vm, "ShutdownVM");
        if let Some(owner) = owner.as_ref() {
            tokio::time::timeout(Duration::from_secs(30), owner.ask(&msg))
                .await
                .map_err(|_| eyre!("timed out shutting down VM {}", msg.vmid))??;
        } else if let Some(vm) = vm {
            tokio::time::timeout(Duration::from_secs(30), vm.ask(&msg))
                .await
                .map_err(|_| eyre!("timed out shutting down VM {}", msg.vmid))??;
        } else {
            return Err(eyre!("VM not found"));
        }
        drop(owner);

        self.state_store
            .delete_vm_state(msg.vmid)
            .await
            .map_err(|error| eyre!("unable to delete durable VM state: {error}"))?;
        self.remove_vm_intent(msg.vmid);
        Ok(ShutdownVMReply)
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

#[cfg(test)]
mod tests {
    use super::persist_create_state;
    use crate::manifest::{Boot, Compute, DesiredState, MANIFEST_VERSION, Metadata, VmManifest};
    use crate::messages::vm::CreateVM;
    use odorobo::cluster_state::{
        ClusterStateStore, MemoryStateStore, PLACEMENT_PREFIX, PlacementRecord, StateStore,
        VM_MANIFESTS_PREFIX, key,
    };
    use std::sync::Arc;
    use ulid::Ulid;

    #[tokio::test]
    async fn create_state_is_complete_before_dispatch() {
        let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ULID");
        let manifest = VmManifest {
            api_version: MANIFEST_VERSION,
            id: vmid,
            desired: DesiredState {
                metadata: Metadata {
                    name: "test".to_owned(),
                    ..Default::default()
                },
                compute: Compute {
                    vcpus: 1,
                    memory_bytes: 1,
                    ..Default::default()
                },
                boot: Boot::default(),
                ..Default::default()
            },
            observed: None,
        };
        let store = Arc::new(StateStore::Memory(MemoryStateStore::default()));
        persist_create_state(
            &store,
            &CreateVM {
                vmid,
                config: manifest.clone(),
            },
            &PlacementRecord {
                vmid,
                node: "node-a".to_owned(),
            },
        )
        .await
        .expect("state should persist");

        assert_eq!(
            store
                .get::<VmManifest>(&key(VM_MANIFESTS_PREFIX, &vmid))
                .await
                .unwrap(),
            Some(manifest)
        );
        assert!(
            store
                .get::<PlacementRecord>(&key(PLACEMENT_PREFIX, &vmid))
                .await
                .unwrap()
                .is_some()
        );
        drop(store);
    }
}
