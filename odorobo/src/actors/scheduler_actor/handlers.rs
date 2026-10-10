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
    ClusterStateStore, PlacementLifecycle, PlacementRecord, StateError, StateStore, StopIntent,
};

use super::{CachedActorKind, CachedVMActor, SchedulerActor, VmLifecycle, VmPlacement};

fn decode_manifest_values(
    records: Vec<(String, serde_json::Value)>,
) -> std::result::Result<Vec<(String, crate::manifest::VmManifest)>, Report> {
    records
        .into_iter()
        .map(|(key, value)| {
            serde_json::from_value(value)
                .map(|manifest| (key, manifest))
                .map_err(|error| eyre!("unable to decode durable VM manifest: {error}"))
        })
        .collect()
}

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

impl SchedulerActor {
    async fn stop_vm_durably(
        &mut self,
        vmid: ulid::Ulid,
        intent: StopIntent,
    ) -> Result<Option<PlacementRecord>, Report> {
        let paired_manifest = self
            .state_store
            .get::<crate::manifest::VmManifest>(&odorobo::cluster_state::key(
                odorobo::cluster_state::VM_MANIFESTS_PREFIX,
                &vmid,
            ))
            .await
            .map_err(|error| eyre!("unable to read paired VM manifest before stop: {error}"))?;
        let placement = match self.state_store.begin_vm_stop(vmid, intent).await {
            Ok(placement) => placement,
            Err(StateError::Missing) if intent == StopIntent::Delete => {
                self.remove_vm_intent(vmid);
                return Ok(None);
            }
            Err(error) => {
                return Err(eyre!("unable to persist VM stop intent: {error}"));
            }
        };
        let manifest = paired_manifest.or_else(|| self.vm_manifests.get(&vmid).cloned());
        self.vm_manifests.remove(&vmid);
        if let Some(manifest) = manifest {
            self.stop_manifests.insert(vmid, manifest);
        }
        self.durable_placements.insert(vmid, placement.clone());
        self.invalidate_pending_resources();

        let owner = self
            .agent_data_cache
            .values()
            .find(|agent| agent.data.hostname == placement.node)
            .map(|agent| agent.actor_ref.clone())
            .ok_or_else(|| {
                eyre!(
                    "VM owner {} is unavailable; durable stop intent was retained",
                    placement.node
                )
            })?;

        let expected = placement.stop_fence();
        match placement.lifecycle {
            PlacementLifecycle::Deleting => {
                let reply = tokio::time::timeout(
                    Duration::from_secs(30),
                    owner.ask(&DeleteVM {
                        vmid,
                        expected: Some(expected.clone()),
                    }),
                )
                .await
                .map_err(|_| eyre!("timed out deleting VM {vmid}"))??;
                if let Some(error) = reply.error {
                    return Err(eyre!("unable to delete VM: {error}"));
                }
                if reply.completed.as_ref() != Some(&expected) {
                    return Err(eyre!("agent acknowledged a different VM stop incarnation"));
                }
            }
            PlacementLifecycle::Stopping => {
                let reply = tokio::time::timeout(
                    Duration::from_secs(30),
                    owner.ask(&ShutdownVM {
                        vmid,
                        expected: Some(expected.clone()),
                    }),
                )
                .await
                .map_err(|_| eyre!("timed out shutting down VM {vmid}"))?
                .map_err(|error| eyre!("unable to shut down VM: {error}"))?;
                if reply.completed != expected {
                    return Err(eyre!("agent acknowledged a different VM stop incarnation"));
                }
            }
            PlacementLifecycle::Active => {
                return Err(eyre!("VM stop intent unexpectedly remained active"));
            }
        }
        drop(owner);

        self.state_store
            .complete_vm_stop(&placement)
            .await
            .map_err(|error| eyre!("unable to finalize durable VM stop: {error}"))?;
        self.remove_vm_intent(vmid);
        Ok(Some(placement))
    }

    async fn adopt_persisted_create(&mut self, msg: &CreateVM) -> Result<CreateVMReply, Report> {
        let (manifest_values, placement_records) =
            self.state_store.list_vm_state().await.map_err(|error| {
                eyre!("unable to verify paired create state after write error: {error}")
            })?;
        let manifest = decode_manifest_values(manifest_values)?
            .into_iter()
            .map(|(_, manifest)| manifest)
            .find(|manifest| manifest.id == msg.vmid);
        let placement = placement_records
            .into_iter()
            .map(|(_, placement)| placement)
            .find(|placement| placement.vmid == msg.vmid);
        let (Some(manifest), Some(placement)) = (manifest, placement) else {
            return Err(eyre!("VM create state was not durably committed"));
        };
        if manifest != msg.config {
            return Err(eyre!("conflicting create request for existing VM ID"));
        }
        if placement.lifecycle != PlacementLifecycle::Active {
            return Err(eyre!("VM is already stopping or deleted"));
        }
        self.vm_manifests.insert(msg.vmid, manifest.clone());
        self.stop_manifests.remove(&msg.vmid);
        self.durable_placements.insert(msg.vmid, placement);
        self.invalidate_pending_resources();
        let actor_id = self
            .vm_actorid_ulid_map
            .iter()
            .find_map(|(actor_id, vmid)| (*vmid == msg.vmid).then(|| actor_id.to_bytes()));
        Ok(CreateVMReply {
            config: Some(manifest),
            actor_id,
        })
    }

    /// Refresh desired intent from the authoritative store. Manager-local maps
    /// are only caches and must not cause stale create or reassignment requests.
    pub(super) async fn refresh_durable_state(&mut self) -> Result<(), Report> {
        let (manifest_values, placement_records) = self
            .state_store
            .list_vm_state()
            .await
            .map_err(|error| eyre!("unable to refresh paired durable VM state: {error}"))?;
        let manifests: AHashMap<_, _> = decode_manifest_values(manifest_values)?
            .into_iter()
            .map(|(_, manifest)| (manifest.id, manifest))
            .collect();
        let placements: AHashMap<_, _> = placement_records
            .into_iter()
            .map(|(_, placement)| (placement.vmid, placement))
            .collect();
        // A manifest without a placement is not a committed VM create. This is
        // essential after a concurrent delete; never adopt a torn pair as active.
        let active_manifests: AHashMap<_, _> = manifests
            .iter()
            .filter(|(vmid, _)| {
                placements
                    .get(vmid)
                    .is_some_and(|placement| placement.lifecycle == PlacementLifecycle::Active)
            })
            .map(|(vmid, manifest)| (*vmid, manifest.clone()))
            .collect();
        let stop_manifests: AHashMap<_, _> = manifests
            .into_iter()
            .filter(|(vmid, _)| {
                placements
                    .get(vmid)
                    .is_some_and(|placement| placement.lifecycle != PlacementLifecycle::Active)
            })
            .collect();

        let stale_vmids: Vec<_> = self
            .vm_manifests
            .keys()
            .filter(|vmid| !active_manifests.contains_key(vmid))
            .copied()
            .collect();
        for vmid in stale_vmids {
            self.remove_vm_intent(vmid);
        }
        self.vm_manifests = active_manifests;
        self.stop_manifests = stop_manifests;
        self.durable_placements = placements;
        self.invalidate_pending_resources();
        Ok(())
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

        let (manifest_values, placement_records) = args
            .list_vm_state()
            .await
            .map_err(|error| eyre!("unable to load paired durable VM state: {error}"))?;
        let all_manifests: AHashMap<ulid::Ulid, crate::manifest::VmManifest> =
            decode_manifest_values(manifest_values)?
                .into_iter()
                .map(|(_, manifest)| (manifest.id, manifest))
                .collect();
        let durable_placements: AHashMap<_, _> = placement_records
            .into_iter()
            .map(|(_, placement)| (placement.vmid, placement))
            .collect();
        let vm_manifests = all_manifests
            .iter()
            .filter(|(vmid, _)| {
                durable_placements
                    .get(vmid)
                    .is_some_and(|placement| placement.lifecycle == PlacementLifecycle::Active)
            })
            .map(|(vmid, manifest)| (*vmid, manifest.clone()))
            .collect();
        let stop_manifests = all_manifests
            .into_iter()
            .filter(|(vmid, _)| {
                durable_placements
                    .get(vmid)
                    .is_some_and(|placement| placement.lifecycle != PlacementLifecycle::Active)
            })
            .collect();

        let mut scheduler_actor = Self {
            agent_data_cache: AHashMap::new(),
            agent_keepalive_tasks: AHashMap::new(),
            vm_actorid_ulid_map: AHashMap::new(),
            vm_manifests,
            stop_manifests,
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
        self.refresh_durable_state().await?;
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
            .map(|agent| agent.data.hostname.clone())
            .ok_or_else(|| eyre!("selected agent has no cached hostname"))?;
        let placement = PlacementRecord::active(msg.vmid, node);
        let mut create = msg.clone();
        create.generation = placement.generation;

        // Record desired state before creating anything. A failed/ambiguous
        // transaction is resolved by reading both authoritative records below.
        if let Err(write_error) = persist_create_state(&self.state_store, &create, &placement).await
        {
            return self
                .adopt_persisted_create(&create)
                .await
                .map_err(|adopt_error| {
                    eyre!("{write_error}; unable to adopt committed create state: {adopt_error}")
                });
        }

        self.vm_manifests.insert(msg.vmid, msg.config.clone());
        self.stop_manifests.remove(&msg.vmid);
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

        let reply = tokio::time::timeout(Duration::from_secs(30), target_agent.ask(&create))
            .await
            .map_err(|_| eyre!("timed out creating VM {}", msg.vmid))?;

        if let Ok(reply) = &reply {
            let actor_id_bytes = reply
                .actor_id
                .as_deref()
                .ok_or_else(|| eyre!("agent rejected or did not create VM {}", msg.vmid))?;
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
        tracing::trace!(vmid = %msg.vmid, "DeleteVM");
        let placement = self.stop_vm_durably(msg.vmid, StopIntent::Delete).await?;
        Ok(DeleteVMReply {
            error: None,
            completed: placement.map(|placement| placement.stop_fence()),
        })
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
        tracing::trace!(vmid = %msg.vmid, "ShutdownVM");
        let placement = self
            .stop_vm_durably(msg.vmid, StopIntent::Shutdown)
            .await?
            .ok_or_else(|| eyre!("VM not found"))?;
        Ok(ShutdownVMReply {
            completed: placement.stop_fence(),
        })
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
    use super::{SchedulerActor, persist_create_state};
    use crate::manifest::{Boot, Compute, DesiredState, MANIFEST_VERSION, Metadata, VmManifest};
    use crate::messages::vm::CreateVM;
    use ahash::AHashMap;
    use odorobo::cluster_state::{
        ClusterStateStore, MemoryStateStore, PLACEMENT_PREFIX, PlacementRecord, StateStore,
        StopIntent, VM_MANIFESTS_PREFIX, key,
    };
    use std::sync::Arc;
    use ulid::Ulid;

    fn test_manifest(vmid: Ulid) -> VmManifest {
        VmManifest {
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
        }
    }

    fn test_scheduler(state_store: &Arc<StateStore>) -> SchedulerActor {
        SchedulerActor {
            agent_data_cache: AHashMap::new(),
            agent_keepalive_tasks: AHashMap::new(),
            vm_actorid_ulid_map: AHashMap::new(),
            vm_manifests: AHashMap::new(),
            stop_manifests: AHashMap::new(),
            vm_placements: AHashMap::new(),
            durable_placements: AHashMap::new(),
            vm_data_cache: AHashMap::new(),
            vm_keepalive_tasks: AHashMap::new(),
            pending_resources_cache: None,
            agent_vm_index: AHashMap::new(),
            actor_kinds: AHashMap::new(),
            cache_actor_finder: None,
            state_store: Arc::clone(state_store),
        }
    }

    #[tokio::test]
    async fn refresh_keeps_stopping_manifest_for_restart_safe_accounting() {
        let vmid = Ulid::generate();
        let manifest = test_manifest(vmid);
        let placement = PlacementRecord::active(vmid, "node-a".to_owned());
        let state_store = Arc::new(StateStore::Memory(MemoryStateStore::default()));
        state_store
            .create_vm_state(vmid, &manifest, &placement)
            .await
            .expect("persist active create pair");

        let mut scheduler = test_scheduler(&state_store);
        scheduler
            .refresh_durable_state()
            .await
            .expect("load active state");
        assert!(scheduler.vm_manifests.contains_key(&vmid));
        assert!(!scheduler.stop_manifests.contains_key(&vmid));

        let stopping = state_store
            .begin_vm_stop(vmid, StopIntent::Shutdown)
            .await
            .expect("persist stop intent");
        scheduler
            .refresh_durable_state()
            .await
            .expect("load unresolved stop");
        assert!(!scheduler.vm_manifests.contains_key(&vmid));
        assert_eq!(scheduler.stop_manifests.get(&vmid), Some(&manifest));
        assert_eq!(
            scheduler.durable_placements[&vmid].stop_fence(),
            stopping.stop_fence()
        );

        // A new manager reads the still-paired manifest and stop fence after the
        // prior manager's stop request times out; repeated refresh retains both.
        let mut restarted = test_scheduler(&state_store);
        restarted
            .refresh_durable_state()
            .await
            .expect("recover unresolved stop after restart");
        restarted
            .refresh_durable_state()
            .await
            .expect("retain timed-out stop on later reconciliation");
        assert!(!restarted.vm_manifests.contains_key(&vmid));
        assert_eq!(restarted.stop_manifests.get(&vmid), Some(&manifest));
        assert_eq!(
            restarted.durable_placements[&vmid].stop_fence(),
            stopping.stop_fence()
        );
        drop(restarted);
        drop(scheduler);
        drop(state_store);
    }

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
                generation: ulid::Ulid::nil(),
                config: manifest.clone(),
            },
            &PlacementRecord::active(vmid, "node-a".to_owned()),
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

    #[tokio::test]
    async fn stale_manager_refresh_discards_vm_deleted_by_another_manager() {
        let vmid = Ulid::generate();
        let manifest = test_manifest(vmid);
        let placement = PlacementRecord::active(vmid, "node-a".to_owned());
        let store = Arc::new(StateStore::Memory(MemoryStateStore::default()));
        store
            .create_vm_state(vmid, &manifest, &placement)
            .await
            .expect("create shared durable VM state");

        let mut stale_manager = test_scheduler(&store);
        stale_manager.vm_manifests.insert(vmid, manifest);
        stale_manager.durable_placements.insert(vmid, placement);

        // Manager A confirms teardown and finalizes; manager B still holds the
        // old maps until its authoritative refresh.
        let stopping = store
            .begin_vm_stop(vmid, super::StopIntent::Delete)
            .await
            .unwrap();
        store.complete_vm_stop(&stopping).await.unwrap();
        drop(store);
        stale_manager
            .refresh_durable_state()
            .await
            .expect("refresh etcd intent");

        assert!(!stale_manager.vm_manifests.contains_key(&vmid));
        assert!(!stale_manager.durable_placements.contains_key(&vmid));
        drop(stale_manager);
    }

    #[tokio::test]
    async fn concurrent_delete_refresh_never_adopts_a_torn_manifest_without_placement() {
        let vmid = Ulid::generate();
        let manifest = test_manifest(vmid);
        let placement = PlacementRecord::active(vmid, "node-a".to_owned());
        let store = Arc::new(StateStore::Memory(MemoryStateStore::default()));
        store
            .create_vm_state(vmid, &manifest, &placement)
            .await
            .expect("create shared durable VM state");
        let mut scheduler = test_scheduler(&store);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let delete_store = Arc::clone(&store);
        let delete_barrier = Arc::clone(&barrier);
        let delete = tokio::spawn(async move {
            delete_barrier.wait().await;
            let stopping = delete_store
                .begin_vm_stop(vmid, super::StopIntent::Delete)
                .await
                .unwrap();
            delete_store.complete_vm_stop(&stopping).await.unwrap();
        });
        barrier.wait().await;
        scheduler
            .refresh_durable_state()
            .await
            .expect("refresh a consistent paired snapshot");
        delete.await.expect("finalize concurrent deletion");

        if scheduler.vm_manifests.contains_key(&vmid) {
            assert_eq!(
                scheduler.durable_placements[&vmid].lifecycle,
                odorobo::cluster_state::PlacementLifecycle::Active
            );
        } else if let Some(placement) = scheduler.durable_placements.get(&vmid) {
            assert_ne!(
                placement.lifecycle,
                odorobo::cluster_state::PlacementLifecycle::Active
            );
        }
        let snapshot = store.list_vm_state().await.unwrap();
        assert!(snapshot.0.is_empty());
        assert!(snapshot.1.is_empty());
        drop(store);
        drop(scheduler);
    }

    #[tokio::test]
    async fn ambiguous_create_write_adopts_existing_manifest_and_original_owner() {
        let vmid = Ulid::generate();
        let manifest = test_manifest(vmid);
        let placement = PlacementRecord::active(vmid, "node-original".to_owned());
        let original_generation = placement.generation;
        let store = Arc::new(StateStore::Memory(MemoryStateStore::default()));
        store
            .create_vm_state(vmid, &manifest, &placement)
            .await
            .expect("simulate a committed transaction with a lost response");
        let mut scheduler = test_scheduler(&store);
        drop(store);

        let reply = scheduler
            .adopt_persisted_create(&CreateVM {
                vmid,
                generation: Ulid::nil(),
                config: manifest.clone(),
            })
            .await
            .expect("identical committed state should be adopted");
        assert_eq!(reply.config, Some(manifest.clone()));
        assert_eq!(scheduler.vm_manifests.get(&vmid), Some(&manifest));
        let adopted = scheduler
            .durable_placements
            .get(&vmid)
            .expect("original placement should be retained");
        assert_eq!(adopted.node, "node-original");
        assert_eq!(adopted.generation, original_generation);

        let mut conflict = manifest;
        conflict.desired.metadata.name = "different".to_owned();
        let error = scheduler
            .adopt_persisted_create(&CreateVM {
                vmid,
                generation: Ulid::nil(),
                config: conflict,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("conflicting create"));
        drop(scheduler);
    }
}
