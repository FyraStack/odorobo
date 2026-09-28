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
use ulid::Ulid;

use crate::ch_driver::VMInstance;
use crate::ch_driver::actor::VMActor;
use crate::messages::vm::{
    AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
    GetConsoleHistory, GetConsoleHistoryReply, SendConsoleInput, SendConsoleInputReply, ShutdownVM,
    ShutdownVMReply,
};
use crate::messages::{Ping, Pong};
use crate::utils::actor_names::vm_actor_id;
use odorobo::cluster_state::{PlacementRecord, StateStore};

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

async fn wait_for_vm_shutdown(vmid: Ulid) -> Result<(), Report> {
    tokio::time::timeout(Duration::from_secs(30), async {
        while VMInstance::is_running(&vmid.to_string()).await {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| eyre!("timed out waiting for VM {vmid} to stop"))?;
    Ok(())
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

        let mut scheduler_actor = Self {
            agent_data_cache: AHashMap::new(),
            agent_keepalive_tasks: AHashMap::new(),
            vm_actorid_ulid_map: AHashMap::new(),
            vm_manifests: AHashMap::new(),
            vm_placements: AHashMap::new(),
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

        // todo: attempt vm restarts if necessary.

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

        // TODO: Define duplicate VM-ID semantics before overwriting intent and
        // appending another pending placement; reject conflicts or make retries idempotent.
        self.vm_manifests.insert(msg.vmid, msg.config.clone());
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
            if !actor_exists && let Err(error) = self.state_store.delete_vm_state(msg.vmid).await {
                warn!(?error, vm_id = %msg.vmid, "Unable to roll back VM state after failed create");
            }
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

/// Looks up a VM actor and forwards deletion without eagerly altering scheduler caches.
///
/// Cache cleanup waits for actor link death or updater reachability failure.
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
            // don't update cache, because we rely on link dying and updater task to remove from cache once the VM is fully down.
            vm.ask(&msg).await?;
            wait_for_vm_shutdown(msg.vmid).await?;
            if let Err(error) = self.state_store.delete_vm_state(msg.vmid).await {
                return Err(eyre!("unable to delete durable VM state: {error}"));
            }
            Ok(DeleteVMReply)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

/// Looks up a VM actor and forwards shutdown without eagerly altering scheduler caches.
///
/// Cache cleanup waits for actor link death or updater reachability failure.
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
            // don't update cache, because we rely on link dying and updater task to remove from cache once the VM is fully down.
            vm.tell(&msg).send()?;
            Ok(ShutdownVMReply)
        } else {
            Err(eyre!("VM not found"))
        }
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
