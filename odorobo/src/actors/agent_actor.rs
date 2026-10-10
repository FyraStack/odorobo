use crate::{
    ch_driver::actor::VMActor,
    config::Config,
    manifest::VmManifest,
    messages::{
        Ping, Pong,
        agent::{
            AgentStatus, AgentStatusUpdate, GetAgentStatus, MembershipChange,
            STATUS_CHANGE_HISTORY_LIMIT, StatusChangeHistory, VMResourceCharge,
        },
        debug::PanicAgent,
        vm::{
            AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
            GetVMInfo, GetVMInfoReply, MigrateVMReceive, MigrateVMReceiveReply, ShutdownVM,
            ShutdownVMReply,
        },
    },
    networking::actor::NetworkAgentActor,
    types::ObjectMetadata,
    utils::actor_names::{NETWORK, VM, vm_actor_id},
};
use ahash::{AHashMap, AHashSet};
use bytesize::ByteSize;
use kameo::prelude::*;
use odorobo::cluster_state::{
    ClusterStateStore, PLACEMENT_PREFIX, PlacementLifecycle, PlacementRecord, StateStore,
    VM_MANIFESTS_PREFIX, VMStopFence, key,
};
use stable_eyre::{Report, Result, eyre::eyre};
use std::{
    future::Future,
    ops::ControlFlow,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};
use sysinfo::System;
use tracing::{error, info, trace, warn};
use ulid::Ulid;

use kameo::error::PanicError;

// Leave time for the scheduler's 30-second outer delete request to receive this failure.
const VM_ACTOR_DELETE_TIMEOUT: Duration = Duration::from_secs(25);

fn stop_fence_matches_placement(
    placement: &PlacementRecord,
    fence: &VMStopFence,
    hostname: &str,
) -> bool {
    placement.lifecycle != PlacementLifecycle::Active
        && placement.node == hostname
        && placement.stop_fence() == *fence
}

fn failed_stop_reservation(
    expected: &PlacementRecord,
    current: &PlacementRecord,
    manifest: &VmManifest,
    hostname: &str,
) -> Option<StopResourceReservation> {
    stop_fence_matches_placement(current, &expected.stop_fence(), hostname).then_some(
        StopResourceReservation {
            generation: expected.generation,
            vcpus: manifest.desired.compute.vcpus,
            memory_bytes: manifest.desired.compute.memory_bytes,
        },
    )
}

fn can_track_pending_stop_actor(current: Option<ActorId>, candidate: ActorId) -> bool {
    current.is_none_or(|current| current == candidate)
}

fn cached_predecessor_reservation(
    cached_actor: ActorId,
    cached_generation: Ulid,
    vcpus: u32,
    memory_bytes: u64,
    replacement_actor: ActorId,
    generation: Ulid,
) -> Option<StopResourceReservation> {
    (cached_actor != replacement_actor && cached_generation == generation).then_some(
        StopResourceReservation {
            generation,
            vcpus,
            memory_bytes,
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StopResourceReservation {
    generation: Ulid,
    vcpus: u32,
    memory_bytes: u64,
}

fn total_stop_reservations(resources: &AHashMap<Ulid, StopResourceReservation>) -> (u32, u64) {
    resources
        .values()
        .fold((0_u32, 0_u64), |(vcpus, memory), reservation| {
            (
                vcpus.saturating_add(reservation.vcpus),
                memory.saturating_add(reservation.memory_bytes),
            )
        })
}

type RegistrationLock = Arc<tokio::sync::Mutex<()>>;

static REGISTRATION_LOCKS: OnceLock<
    StdMutex<std::collections::HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
> = OnceLock::new();

fn registration_lock(name: &str) -> RegistrationLock {
    let mut locks = REGISTRATION_LOCKS
        .get_or_init(|| StdMutex::new(std::collections::HashMap::new()))
        .lock()
        .expect("actor registration lock map is not poisoned");
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(name).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(name.to_owned(), Arc::downgrade(&lock));
    lock
}

async fn run_ordered_registration<E, Continue, Register, Fut>(
    name: String,
    lock: RegistrationLock,
    mut should_continue: Continue,
    mut register: Register,
) where
    E: std::fmt::Debug,
    Continue: FnMut() -> bool,
    Register: FnMut() -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    let _guard = lock.lock().await;
    loop {
        if !should_continue() {
            break;
        }
        match register().await {
            Ok(()) => break,
            Err(error) if !should_continue() => {
                warn!(?error, %name, "VM actor stopped while registration was pending");
                break;
            }
            Err(error) => {
                warn!(?error, %name, "Unable to register VM actor; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn after_confirmed_teardown<T, E, Stop, Make, Start>(
    teardown: Stop,
    start_replacement: Make,
) -> Result<T, E>
where
    Stop: Future<Output = Result<(), E>>,
    Make: FnOnce() -> Start,
    Start: Future<Output = T>,
{
    teardown.await?;
    Ok(start_replacement().await)
}

pub struct VMCacheData {
    actor_ref: ActorRef<VMActor>,
    config: VmManifest,
    vcpus: u32,
    memory_bytes: u64,
    generation: Ulid,
}

#[derive(Clone)]
struct PendingStopActor {
    actor_ref: ActorRef<VMActor>,
    generation: Ulid,
    owner: String,
}

struct StopRecoveryFailure {
    error: String,
    actor_ref: Option<ActorRef<VMActor>>,
}

#[derive(Debug)]
enum ActorShutdownError {
    TimedOut,
    Failed(String),
}

enum CachedActorDisposition {
    Keep,
    PreserveUnverified(String),
    PreserveMismatch,
    Teardown,
}

#[derive(RemoteActor)]
pub struct AgentActor {
    pub vcpus: u32,
    pub memory: ByteSize,
    used_vcpus: u32,
    used_memory_bytes: u64,
    membership_revision: u64,
    status_history: StatusChangeHistory,
    pub config: Config,
    pub vms: AHashMap<Ulid, VMCacheData>,
    /// Resource reservations for local VMs whose durable stop has not yet been
    /// confirmed. They remain charged even when no actor handle can be recovered.
    pending_stop_resources: AHashMap<Ulid, StopResourceReservation>,
    pending_stop_actors: AHashMap<Ulid, PendingStopActor>,
    // pub network_actor: ActorRef<NetworkAgentActor>,
    pub metadata: ObjectMetadata,
    pub state_store: Arc<StateStore>,
}

impl AgentActor {
    fn manifests_for_node(
        placements: Vec<(String, PlacementRecord)>,
        records: Vec<(String, VmManifest)>,
        hostname: &str,
    ) -> Vec<VmManifest> {
        let local_vmids: AHashSet<_> = placements
            .into_iter()
            .map(|(_, placement)| placement)
            .filter(|placement| {
                placement.node == hostname && placement.lifecycle == PlacementLifecycle::Active
            })
            .map(|placement| placement.vmid)
            .collect();
        records
            .into_iter()
            .map(|(_, manifest)| manifest)
            .filter(|manifest| local_vmids.contains(&manifest.id))
            .collect()
    }

    #[cfg(test)]
    fn recovered_resources(manifests: &[VmManifest]) -> (u32, u64) {
        manifests
            .iter()
            .fold((0, 0), |(vcpus, memory_bytes), manifest| {
                (
                    vcpus.saturating_add(manifest.desired.compute.vcpus),
                    memory_bytes.saturating_add(manifest.desired.compute.memory_bytes),
                )
            })
    }

    const fn advance_status_revision(&mut self) {
        self.membership_revision = self.membership_revision.saturating_add(1);
    }

    fn record_membership_change(&mut self, vmid: Ulid, added: bool) {
        self.advance_status_revision();
        self.status_history.push_back(MembershipChange {
            revision: self.membership_revision,
            vmid,
            added,
        });
        while self.status_history.len() > STATUS_CHANGE_HISTORY_LIMIT {
            self.status_history.pop_front();
        }
    }

    fn charge_running_resources(&mut self, vmid: Ulid, vcpus: u32, memory_bytes: u64) -> bool {
        // A pending stop reservation and an adopted/spawned actor represent the
        // same VM allocation. Transfer the charge instead of adding it twice.
        let transferred = if let Some(reservation) = self.pending_stop_resources.remove(&vmid) {
            self.used_vcpus = self.used_vcpus.saturating_sub(reservation.vcpus);
            self.used_memory_bytes = self
                .used_memory_bytes
                .saturating_sub(reservation.memory_bytes);
            true
        } else {
            false
        };
        self.used_vcpus = self.used_vcpus.saturating_add(vcpus);
        self.used_memory_bytes = self.used_memory_bytes.saturating_add(memory_bytes);
        transferred
    }

    const fn release_running_resources(&mut self, vcpus: u32, memory_bytes: u64) {
        self.used_vcpus = self.used_vcpus.saturating_sub(vcpus);
        self.used_memory_bytes = self.used_memory_bytes.saturating_sub(memory_bytes);
    }

    fn insert_vm(&mut self, vmid: Ulid, cache: VMCacheData) {
        let generation = cache.generation;
        let vcpus = cache.vcpus;
        let memory_bytes = cache.memory_bytes;
        let previous = self.vms.insert(vmid, cache);
        if let Some(previous) = &previous {
            self.release_running_resources(previous.vcpus, previous.memory_bytes);
            if previous.generation != generation
                || previous.vcpus != vcpus
                || previous.memory_bytes != memory_bytes
            {
                self.advance_status_revision();
            }
        } else {
            self.record_membership_change(vmid, true);
        }
        if self.charge_running_resources(vmid, vcpus, memory_bytes) && previous.is_some() {
            self.advance_status_revision();
        }
    }

    fn remove_vm(&mut self, vmid: Ulid) -> Option<VMCacheData> {
        let removed = self.vms.remove(&vmid)?;
        self.record_membership_change(vmid, false);
        self.release_running_resources(removed.vcpus, removed.memory_bytes);
        Some(removed)
    }

    /// Retires the failed cached predecessor when a replacement actor takes over
    /// cleanup. Transfer its capacity charge into the unresolved stop reservation
    /// before removing it, so a delayed link notification cannot recreate an old
    /// reservation after the replacement confirms teardown.
    fn retire_cached_predecessor(
        &mut self,
        vmid: Ulid,
        generation: Ulid,
        replacement_actor_id: ActorId,
    ) {
        let Some(cached) = self.vms.get(&vmid) else {
            return;
        };
        let Some(reservation) = cached_predecessor_reservation(
            cached.actor_ref.id(),
            cached.generation,
            cached.vcpus,
            cached.memory_bytes,
            replacement_actor_id,
            generation,
        ) else {
            return;
        };
        self.reserve_stop_resources(
            vmid,
            reservation.generation,
            reservation.vcpus,
            reservation.memory_bytes,
        );
        self.remove_vm(vmid);
    }

    fn reserve_stop_resources(
        &mut self,
        vmid: Ulid,
        generation: Ulid,
        vcpus: u32,
        memory_bytes: u64,
    ) {
        let next = StopResourceReservation {
            generation,
            vcpus,
            memory_bytes,
        };
        let previous = self.pending_stop_resources.insert(vmid, next);
        if previous != Some(next) {
            if let Some(previous) = previous {
                self.used_vcpus = self.used_vcpus.saturating_sub(previous.vcpus);
                self.used_memory_bytes =
                    self.used_memory_bytes.saturating_sub(previous.memory_bytes);
            }
            self.used_vcpus = self.used_vcpus.saturating_add(vcpus);
            self.used_memory_bytes = self.used_memory_bytes.saturating_add(memory_bytes);
            self.advance_status_revision();
        }
    }

    fn release_stop_resources(&mut self, vmid: Ulid) {
        if let Some(reservation) = self.pending_stop_resources.remove(&vmid) {
            self.used_vcpus = self.used_vcpus.saturating_sub(reservation.vcpus);
            self.used_memory_bytes = self
                .used_memory_bytes
                .saturating_sub(reservation.memory_bytes);
            self.advance_status_revision();
        }
    }

    async fn lookup_vm_actor(vmid: Ulid) -> Option<ActorRef<VMActor>> {
        ActorRef::<VMActor>::lookup(vm_actor_id(vmid))
            .await
            .ok()
            .flatten()
    }

    fn status_snapshot(&self) -> AgentStatus {
        AgentStatus {
            hostname: self.config.get_hostname().to_owned(),
            vcpus: self.vcpus,
            ram: self.memory,
            used_vcpus: self
                .used_vcpus
                .saturating_add(self.config.get_reserved_vcpus()),
            used_ram: ByteSize::b(self.used_memory_bytes),
            vms: {
                let mut vms: Vec<_> = self.vms.keys().copied().collect();
                vms.sort_unstable();
                vms
            },
            reserved_vms: {
                let mut vms: Vec<_> = self.pending_stop_resources.keys().copied().collect();
                vms.sort_unstable();
                vms
            },
            resource_charges: {
                let mut charges: Vec<_> = self
                    .vms
                    .iter()
                    .map(|(vmid, vm)| VMResourceCharge {
                        vmid: *vmid,
                        generation: vm.generation,
                        vcpus: vm.vcpus,
                        memory_bytes: vm.memory_bytes,
                    })
                    .chain(
                        self.pending_stop_resources
                            .iter()
                            .map(|(vmid, reservation)| VMResourceCharge {
                                vmid: *vmid,
                                generation: reservation.generation,
                                vcpus: reservation.vcpus,
                                memory_bytes: reservation.memory_bytes,
                            }),
                    )
                    .collect();
                charges.sort_unstable_by_key(|charge| charge.vmid);
                charges
            },
            metadata: self.metadata.clone(),
        }
    }

    fn track_pending_stop_actor(
        &mut self,
        vmid: Ulid,
        actor_ref: ActorRef<VMActor>,
        generation: Ulid,
        owner: String,
    ) {
        let actor_id = actor_ref.id();
        if can_track_pending_stop_actor(
            self.pending_stop_actors
                .get(&vmid)
                .map(|current| current.actor_ref.id()),
            actor_id,
        ) {
            self.pending_stop_actors.insert(
                vmid,
                PendingStopActor {
                    actor_ref,
                    generation,
                    owner,
                },
            );
        }
    }

    fn handoff_pending_stop_actor(
        &mut self,
        vmid: Ulid,
        actor_ref: ActorRef<VMActor>,
        generation: Ulid,
        owner: String,
    ) {
        self.pending_stop_actors.insert(
            vmid,
            PendingStopActor {
                actor_ref,
                generation,
                owner,
            },
        );
    }

    async fn cleanup_runtime_after_actor_failure(
        &mut self,
        parent: &ActorRef<Self>,
        state_store: &StateStore,
        vmid: Ulid,
        generation: Ulid,
        owner: String,
    ) -> Result<(), (String, Option<PendingStopActor>)> {
        let placement = state_store
            .get::<PlacementRecord>(&key(PLACEMENT_PREFIX, &vmid))
            .await
            .map_err(|error| (error.to_string(), None))?;
        if !placement
            .is_some_and(|placement| placement.generation == generation && placement.node == owner)
        {
            return Err((
                "durable placement changed before restart-safe runtime cleanup".to_owned(),
                None,
            ));
        }
        let actor_ref = VMActor::spawn_link(parent, (vmid, None, generation, owner.clone())).await;
        let pending = PendingStopActor {
            actor_ref: actor_ref.clone(),
            generation,
            owner: owner.clone(),
        };
        self.handoff_pending_stop_actor(vmid, actor_ref.clone(), generation, owner.clone());
        self.retire_cached_predecessor(vmid, generation, actor_ref.id());
        if let Err(error) = actor_ref
            .wait_for_startup_with_result(|result| result.map_err(|error| error.to_string()))
            .await
        {
            return Err((
                format!("unable to reattach VM runtime for cleanup: {error}"),
                Some(pending),
            ));
        }
        let expected = VMStopFence {
            generation,
            owner,
            lifecycle: PlacementLifecycle::Deleting,
        };
        Self::teardown_actor(&actor_ref, vmid, expected)
            .await
            .map_err(|error| (error, Some(pending)))
    }

    async fn resolve_stopped_pending_actor(&mut self, vmid: Ulid, parent: &ActorRef<Self>) -> bool {
        let Some(pending) = self.pending_stop_actors.get(&vmid).cloned() else {
            return true;
        };
        if pending.actor_ref.is_alive() {
            return false;
        }
        let shutdown =
            Self::wait_for_shutdown_completion(pending.actor_ref.wait_for_shutdown_with_result(
                |result| result.cloned().map_err(|error| error.to_string()),
            ))
            .await;
        match shutdown {
            Ok(_) => {
                self.retire_cached_predecessor(vmid, pending.generation, pending.actor_ref.id());
                if self
                    .pending_stop_actors
                    .get(&vmid)
                    .is_some_and(|current| current.actor_ref.id() == pending.actor_ref.id())
                {
                    self.pending_stop_actors.remove(&vmid);
                    self.release_stop_resources(vmid);
                }
                true
            }
            Err(ActorShutdownError::TimedOut) => {
                warn!(%vmid, "VM actor shutdown result is still pending; retaining its reservation");
                false
            }
            Err(ActorShutdownError::Failed(error)) => {
                warn!(%error, %vmid, "VM actor cleanup failed during shutdown; reattaching for a bounded runtime cleanup");
                let state_store = Arc::clone(&self.state_store);
                match self
                    .cleanup_runtime_after_actor_failure(
                        parent,
                        &state_store,
                        vmid,
                        pending.generation,
                        pending.owner.clone(),
                    )
                    .await
                {
                    Ok(()) => {
                        self.retire_cached_predecessor(
                            vmid,
                            pending.generation,
                            self.pending_stop_actors.get(&vmid).map_or_else(
                                || pending.actor_ref.id(),
                                |current| current.actor_ref.id(),
                            ),
                        );
                        if self.pending_stop_actors.get(&vmid).is_some_and(|current| {
                            current.generation == pending.generation
                                && current.owner == pending.owner
                        }) {
                            self.pending_stop_actors.remove(&vmid);
                            self.release_stop_resources(vmid);
                        }
                        true
                    }
                    Err((error, cleanup_actor)) => {
                        warn!(%error, %vmid, "Unable to confirm restart-safe VM runtime cleanup; retaining reservation");
                        let retained = cleanup_actor.unwrap_or(pending);
                        self.track_pending_stop_actor(
                            vmid,
                            retained.actor_ref,
                            retained.generation,
                            retained.owner,
                        );
                        false
                    }
                }
            }
        }
    }

    fn create_matches_authoritative_state(
        placement: Option<PlacementRecord>,
        manifest: Option<VmManifest>,
        hostname: &str,
        msg: &CreateVM,
    ) -> bool {
        matches!(
            (placement, manifest),
            (Some(placement), Some(manifest))
                if placement.lifecycle == PlacementLifecycle::Active
                    && placement.node == hostname
                    && placement.generation == msg.generation
                    && manifest == msg.config
        )
    }

    async fn validate_create_against(
        state_store: &StateStore,
        hostname: &str,
        msg: &CreateVM,
    ) -> Result<bool, String> {
        let (manifests, placements) = state_store
            .list_vm_state()
            .await
            .map_err(|error| error.to_string())?;
        let manifest_key = key(VM_MANIFESTS_PREFIX, &msg.vmid);
        let placement_key = key(PLACEMENT_PREFIX, &msg.vmid);
        let placement = placements
            .into_iter()
            .find_map(|(record_key, placement)| (record_key == placement_key).then_some(placement));
        let manifest = manifests
            .into_iter()
            .find_map(|(record_key, value)| (record_key == manifest_key).then_some(value))
            .map(serde_json::from_value::<VmManifest>)
            .transpose()
            .map_err(|error| format!("unable to decode authoritative VM manifest: {error}"))?;
        Ok(Self::create_matches_authoritative_state(
            placement, manifest, hostname, msg,
        ))
    }

    async fn validate_create(&self, msg: &CreateVM) -> Result<bool, String> {
        Self::validate_create_against(&self.state_store, self.config.get_hostname(), msg).await
    }

    fn cached_actor_disposition(
        heartbeat_healthy: bool,
        validation: Result<bool, String>,
    ) -> CachedActorDisposition {
        match (heartbeat_healthy, validation) {
            (true, Ok(true)) => CachedActorDisposition::Keep,
            (_, Err(error)) => CachedActorDisposition::PreserveUnverified(error),
            (_, Ok(false)) => CachedActorDisposition::PreserveMismatch,
            (false, Ok(true)) => CachedActorDisposition::Teardown,
        }
    }

    async fn wait_for_shutdown_completion<F, T, E>(
        shutdown: F,
    ) -> std::result::Result<T, ActorShutdownError>
    where
        F: Future<Output = std::result::Result<T, E>>,
        E: std::fmt::Display,
    {
        tokio::time::timeout(VM_ACTOR_DELETE_TIMEOUT, shutdown)
            .await
            .map_err(|_| ActorShutdownError::TimedOut)?
            .map_err(|error| ActorShutdownError::Failed(error.to_string()))
    }

    async fn teardown_actor(
        actor_ref: &ActorRef<VMActor>,
        vmid: Ulid,
        expected: VMStopFence,
    ) -> Result<(), String> {
        tokio::time::timeout(VM_ACTOR_DELETE_TIMEOUT, async {
            let reply = actor_ref
                .ask(DeleteVM {
                    vmid,
                    expected: Some(expected.clone()),
                })
                .await
                .map_err(|error| error.to_string())?;
            if let Some(error) = reply.error {
                return Err(error);
            }
            if reply.completed.as_ref() != Some(&expected) {
                return Err("VM runtime acknowledged a different stop incarnation".to_owned());
            }
            Self::wait_for_shutdown_completion(actor_ref.wait_for_shutdown_with_result(|result| {
                result.cloned().map_err(|error| error.to_string())
            }))
            .await
            .map_err(|error| format!("VM actor shutdown did not complete cleanly: {error:?}"))?;
            Ok(())
        })
        .await
        .map_err(|_| "timed out waiting for confirmed VM teardown".to_owned())?
    }

    #[allow(clippy::too_many_lines)]
    async fn spawn_vm(
        &mut self,
        msg: &CreateVM,
        ctx: &Context<Self, CreateVMReply>,
    ) -> CreateVMReply {
        let vmid = msg.vmid;
        let owner = self.config.get_hostname().to_owned();
        match self.validate_create(msg).await {
            Ok(true) => {}
            Ok(false) => {
                warn!(%vmid, generation = %msg.generation, "Create intent changed before VM startup");
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
            Err(error) => {
                warn!(%error, %vmid, "Unable to verify create intent before VM startup");
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
        }
        let actor_ref = VMActor::spawn_link(
            ctx.actor_ref(),
            (
                vmid,
                Some(msg.config.clone()),
                msg.generation,
                owner.clone(),
            ),
        )
        .await;
        let startup = actor_ref
            .wait_for_startup_with_result(|result| result.map_err(|error| error.to_string()))
            .await;
        if let Err(error) = startup {
            warn!(%error, %vmid, "Unable to start VM actor; retaining its exact handle for shutdown recovery");
            self.reserve_stop_resources(
                vmid,
                msg.generation,
                msg.config.desired.compute.vcpus,
                msg.config.desired.compute.memory_bytes,
            );
            self.pending_stop_actors.insert(
                vmid,
                PendingStopActor {
                    actor_ref: actor_ref.clone(),
                    generation: msg.generation,
                    owner: owner.clone(),
                },
            );
            if !actor_ref.is_alive() {
                let _: bool = self
                    .resolve_stopped_pending_actor(vmid, ctx.actor_ref())
                    .await;
            }
            return CreateVMReply {
                config: None,
                actor_id: None,
            };
        }
        let expected_active = VMStopFence {
            generation: msg.generation,
            owner: owner.clone(),
            lifecycle: PlacementLifecycle::Stopping,
        };
        match self.validate_create(msg).await {
            Ok(true) => {}
            Ok(false) => {
                if let Err(error) =
                    Self::teardown_actor(&actor_ref, vmid, expected_active.clone()).await
                {
                    warn!(%error, %vmid, "Create intent changed during VM startup; retaining reservation until teardown is confirmed");
                    self.insert_vm(
                        vmid,
                        VMCacheData {
                            actor_ref,
                            config: msg.config.clone(),
                            vcpus: msg.config.desired.compute.vcpus,
                            memory_bytes: msg.config.desired.compute.memory_bytes,
                            generation: msg.generation,
                        },
                    );
                }
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
            Err(error) => {
                warn!(%error, %vmid, "Unable to revalidate create intent after startup; retaining the started actor");
                Self::register_vm_actor(&actor_ref, vmid);
                self.insert_vm(
                    vmid,
                    VMCacheData {
                        actor_ref,
                        config: msg.config.clone(),
                        vcpus: msg.config.desired.compute.vcpus,
                        memory_bytes: msg.config.desired.compute.memory_bytes,
                        generation: msg.generation,
                    },
                );
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
        }
        Self::register_vm_actor(&actor_ref, vmid);
        match self.validate_create(msg).await {
            Ok(true) => {}
            Ok(false) => {
                if let Err(error) = Self::teardown_actor(&actor_ref, vmid, expected_active).await {
                    warn!(%error, %vmid, "Create intent changed during actor registration; retaining reservation until teardown is confirmed");
                    self.insert_vm(
                        vmid,
                        VMCacheData {
                            actor_ref,
                            config: msg.config.clone(),
                            vcpus: msg.config.desired.compute.vcpus,
                            memory_bytes: msg.config.desired.compute.memory_bytes,
                            generation: msg.generation,
                        },
                    );
                }
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
            Err(error) => {
                warn!(%error, %vmid, "Unable to revalidate create intent after registration; retaining the started actor");
                self.insert_vm(
                    vmid,
                    VMCacheData {
                        actor_ref,
                        config: msg.config.clone(),
                        vcpus: msg.config.desired.compute.vcpus,
                        memory_bytes: msg.config.desired.compute.memory_bytes,
                        generation: msg.generation,
                    },
                );
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
        }
        self.insert_vm(
            vmid,
            VMCacheData {
                actor_ref: actor_ref.clone(),
                config: msg.config.clone(),
                vcpus: msg.config.desired.compute.vcpus,
                memory_bytes: msg.config.desired.compute.memory_bytes,
                generation: msg.generation,
            },
        );
        info!(?vmid, "VM Spawned successfully");
        CreateVMReply {
            config: Some(msg.config.clone()),
            actor_id: Some(actor_ref.id().to_bytes()),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn stop_runtime(
        &mut self,
        vmid: Ulid,
        expected: Option<VMStopFence>,
        parent: &ActorRef<Self>,
    ) -> Result<VMStopFence, String> {
        let expected =
            expected.ok_or_else(|| "VM stop request has no incarnation fence".to_owned())?;
        let placement = self
            .state_store
            .get::<PlacementRecord>(&key(PLACEMENT_PREFIX, &vmid))
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "durable VM placement is missing".to_owned())?;
        if !stop_fence_matches_placement(&placement, &expected, self.config.get_hostname()) {
            return Err("VM placement no longer matches the stop incarnation".to_owned());
        }

        if !self.vms.contains_key(&vmid)
            && !self.pending_stop_resources.contains_key(&vmid)
            && let Ok(Some(manifest)) = self
                .state_store
                .get::<VmManifest>(&key(VM_MANIFESTS_PREFIX, &vmid))
                .await
        {
            self.reserve_stop_resources(
                vmid,
                expected.generation,
                manifest.desired.compute.vcpus,
                manifest.desired.compute.memory_bytes,
            );
        }
        let actor_ref = if let Some(actor) = self.pending_stop_actors.get(&vmid) {
            Some(actor.actor_ref.clone())
        } else if let Some(vm) = self.vms.get(&vmid) {
            Some(vm.actor_ref.clone())
        } else {
            Self::lookup_vm_actor(vmid).await
        };
        let actor_ref = if let Some(actor) = actor_ref {
            actor
        } else {
            // Let VMInstance make a bounded, conservative reattach/spawn decision;
            // a non-responsive socket must not be mistaken for a stopped VMM.
            let actor = VMActor::spawn_link(
                parent,
                (vmid, None, expected.generation, expected.owner.clone()),
            )
            .await;
            self.track_pending_stop_actor(
                vmid,
                actor.clone(),
                expected.generation,
                expected.owner.clone(),
            );
            actor
                .wait_for_startup_with_result(|result| result.map_err(|error| error.to_string()))
                .await
                .map_err(|error| format!("unable to attach VM actor for teardown: {error}"))?;
            Self::register_vm_actor(&actor, vmid);
            actor
        };
        self.track_pending_stop_actor(
            vmid,
            actor_ref.clone(),
            expected.generation,
            expected.owner.clone(),
        );
        if !actor_ref.is_alive() {
            let generation = self
                .pending_stop_actors
                .get(&vmid)
                .map_or(expected.generation, |pending| pending.generation);
            match Self::wait_for_shutdown_completion(actor_ref.wait_for_shutdown_with_result(
                |result| result.cloned().map_err(|error| error.to_string()),
            ))
            .await
            {
                Ok(_) => {
                    if self
                        .vms
                        .get(&vmid)
                        .is_some_and(|current| current.actor_ref.id() == actor_ref.id())
                    {
                        self.remove_vm(vmid);
                    }
                    if self
                        .pending_stop_actors
                        .get(&vmid)
                        .is_some_and(|current| current.actor_ref.id() == actor_ref.id())
                    {
                        self.pending_stop_actors.remove(&vmid);
                        self.release_stop_resources(vmid);
                    }
                    return Ok(expected);
                }
                Err(ActorShutdownError::Failed(shutdown_error)) => {
                    warn!(%shutdown_error, %vmid, "VM actor shutdown cleanup failed; reattaching for stop recovery");
                    let state_store = Arc::clone(&self.state_store);
                    self.cleanup_runtime_after_actor_failure(
                        parent,
                        &state_store,
                        vmid,
                        generation,
                        expected.owner.clone(),
                    )
                    .await
                    .map_err(|(error, cleanup_actor)| {
                        if let Some(cleanup_actor) = cleanup_actor {
                            self.track_pending_stop_actor(
                                vmid,
                                cleanup_actor.actor_ref,
                                cleanup_actor.generation,
                                cleanup_actor.owner,
                            );
                        } else {
                            self.track_pending_stop_actor(
                                vmid,
                                actor_ref.clone(),
                                generation,
                                expected.owner.clone(),
                            );
                        }
                        error
                    })?;
                    if self
                        .vms
                        .get(&vmid)
                        .is_some_and(|current| current.actor_ref.id() == actor_ref.id())
                    {
                        self.remove_vm(vmid);
                    }
                    if self
                        .pending_stop_actors
                        .get(&vmid)
                        .is_some_and(|current| current.generation == generation)
                    {
                        self.pending_stop_actors.remove(&vmid);
                        self.release_stop_resources(vmid);
                    }
                    return Ok(expected);
                }
                Err(ActorShutdownError::TimedOut) => {
                    return Err("unconfirmed VM actor shutdown is still pending".to_owned());
                }
            }
        }
        Self::teardown_actor(&actor_ref, vmid, expected.clone()).await?;
        if self
            .vms
            .get(&vmid)
            .is_some_and(|current| current.actor_ref.id() == actor_ref.id())
        {
            self.remove_vm(vmid);
        }
        if self
            .pending_stop_actors
            .get(&vmid)
            .is_some_and(|current| current.actor_ref.id() == actor_ref.id())
        {
            self.pending_stop_actors.remove(&vmid);
        }
        if self
            .pending_stop_actors
            .get(&vmid)
            .is_none_or(|current| current.actor_ref.id() == actor_ref.id())
        {
            self.release_stop_resources(vmid);
        }
        Ok(expected)
    }

    async fn finish_recovered_stop(
        parent: &ActorRef<Self>,
        state_store: &StateStore,
        placement: &PlacementRecord,
    ) -> Result<(), StopRecoveryFailure> {
        let vmid = placement.vmid;
        let expected = placement.stop_fence();
        let current = match state_store
            .get::<PlacementRecord>(&key(PLACEMENT_PREFIX, &vmid))
            .await
        {
            Ok(Some(current)) => current,
            Ok(None) => {
                return Err(StopRecoveryFailure {
                    error: "durable placement disappeared before stop recovery".to_owned(),
                    actor_ref: Self::lookup_vm_actor(vmid).await,
                });
            }
            Err(error) => {
                return Err(StopRecoveryFailure {
                    error: error.to_string(),
                    actor_ref: Self::lookup_vm_actor(vmid).await,
                });
            }
        };
        if !stop_fence_matches_placement(&current, &expected, &placement.node) {
            return Err(StopRecoveryFailure {
                error: "durable placement changed before stop recovery".to_owned(),
                actor_ref: Self::lookup_vm_actor(vmid).await,
            });
        }
        let actor_ref = if let Some(actor) = Self::lookup_vm_actor(vmid).await {
            actor
        } else {
            // Reattach (or safely create a bare VMM) so the driver can determine
            // whether teardown is confirmed. Do not infer stop from a failed ping.
            let actor = VMActor::spawn_link(
                parent,
                (vmid, None, expected.generation, expected.owner.clone()),
            )
            .await;
            if let Err(error) = actor
                .wait_for_startup_with_result(|result| result.map_err(|error| error.to_string()))
                .await
            {
                return Err(StopRecoveryFailure {
                    error: format!("unable to attach VM actor for recovery: {error}"),
                    actor_ref: Some(actor),
                });
            }
            Self::register_vm_actor(&actor, vmid);
            actor
        };
        if let Err(error) = Self::teardown_actor(&actor_ref, vmid, expected).await {
            return Err(StopRecoveryFailure {
                error,
                actor_ref: Some(actor_ref),
            });
        }
        if let Err(error) = state_store.complete_vm_stop(placement).await {
            return Err(StopRecoveryFailure {
                error: error.to_string(),
                actor_ref: None,
            });
        }
        Ok(())
    }

    // This routine deliberately keeps recovery and its capacity-accounting decisions
    // together so an unconfirmed durable stop cannot be lost between phases.
    #[allow(clippy::too_many_lines)]
    async fn recover_vms(
        parent: &ActorRef<Self>,
        state_store: &StateStore,
        hostname: &str,
        placements: Vec<(String, PlacementRecord)>,
        records: Vec<(String, VmManifest)>,
    ) -> (
        AHashMap<Ulid, VMCacheData>,
        AHashMap<Ulid, StopResourceReservation>,
        AHashMap<Ulid, PendingStopActor>,
    ) {
        let manifests_by_id: AHashMap<_, _> = records
            .iter()
            .map(|(_, manifest)| (manifest.id, manifest.clone()))
            .collect();
        let mut vms = AHashMap::new();
        let mut pending_resources = AHashMap::new();
        let mut pending_actors = AHashMap::new();
        for (_, placement) in &placements {
            if placement.node != hostname || placement.lifecycle == PlacementLifecycle::Active {
                continue;
            }
            if let Err(failure) = Self::finish_recovered_stop(parent, state_store, placement).await
            {
                warn!(error = %failure.error, vm_id = %placement.vmid, "Unable to finish durable VM stop during agent startup");
                let reservation = match state_store
                    .get::<PlacementRecord>(&key(PLACEMENT_PREFIX, &placement.vmid))
                    .await
                {
                    Ok(Some(current)) => {
                        manifests_by_id.get(&placement.vmid).and_then(|manifest| {
                            failed_stop_reservation(placement, &current, manifest, hostname)
                        })
                    }
                    Ok(None) => None,
                    Err(_) => manifests_by_id.get(&placement.vmid).map(|manifest| {
                        StopResourceReservation {
                            generation: placement.generation,
                            vcpus: manifest.desired.compute.vcpus,
                            memory_bytes: manifest.desired.compute.memory_bytes,
                        }
                    }),
                };
                if let Some(reservation) = reservation {
                    pending_resources.insert(placement.vmid, reservation);
                    if let Some(actor_ref) = failure.actor_ref {
                        pending_actors.insert(
                            placement.vmid,
                            PendingStopActor {
                                actor_ref,
                                generation: placement.generation,
                                owner: hostname.to_owned(),
                            },
                        );
                    }
                }
            }
        }

        let recovered_manifests = Self::manifests_for_node(placements, records, hostname);
        for vm_config in recovered_manifests {
            let vmid = vm_config.id;
            let placement = match state_store
                .get::<PlacementRecord>(&key(PLACEMENT_PREFIX, &vmid))
                .await
            {
                Ok(Some(placement)) if placement.node == hostname => placement,
                Ok(_) => continue,
                Err(error) => {
                    warn!(?error, %vmid, "Unable to verify recovered VM placement; retaining the initial snapshot reservation");
                    pending_resources.insert(
                        vmid,
                        StopResourceReservation {
                            generation: Ulid::nil(),
                            vcpus: vm_config.desired.compute.vcpus,
                            memory_bytes: vm_config.desired.compute.memory_bytes,
                        },
                    );
                    continue;
                }
            };
            if placement.lifecycle != PlacementLifecycle::Active {
                if let Err(failure) =
                    Self::finish_recovered_stop(parent, state_store, &placement).await
                {
                    warn!(error = %failure.error, %vmid, "Stop intent appeared during agent startup; retaining resource reservation");
                    pending_resources.insert(
                        vmid,
                        StopResourceReservation {
                            generation: placement.generation,
                            vcpus: vm_config.desired.compute.vcpus,
                            memory_bytes: vm_config.desired.compute.memory_bytes,
                        },
                    );
                    if let Some(actor_ref) = failure.actor_ref {
                        pending_actors.insert(
                            vmid,
                            PendingStopActor {
                                actor_ref,
                                generation: placement.generation,
                                owner: hostname.to_owned(),
                            },
                        );
                    }
                }
                continue;
            }
            let create = CreateVM {
                vmid,
                generation: placement.generation,
                config: vm_config.clone(),
            };
            match Self::validate_create_against(state_store, hostname, &create).await {
                Ok(true) => {}
                Ok(false) => {
                    warn!(%vmid, "Skipping recovered VM whose durable intent changed during startup");
                    continue;
                }
                Err(error) => {
                    warn!(%error, %vmid, "Unable to verify recovered VM intent; retaining a resource reservation");
                    pending_resources.insert(
                        vmid,
                        StopResourceReservation {
                            generation: placement.generation,
                            vcpus: vm_config.desired.compute.vcpus,
                            memory_bytes: vm_config.desired.compute.memory_bytes,
                        },
                    );
                    continue;
                }
            }
            if let Some(actor) = Self::lookup_vm_actor(vmid).await {
                let heartbeat = tokio::time::timeout(
                    Duration::from_secs(5),
                    actor.ask(crate::messages::vm::GetVMHeartbeat),
                )
                .await;
                let healthy = heartbeat.as_ref().is_ok_and(|result| {
                    result.as_ref().is_ok_and(|heartbeat| {
                        heartbeat.error.is_none() && heartbeat.generation == placement.generation
                    })
                });
                let validation = if healthy {
                    Self::validate_create_against(state_store, hostname, &create).await
                } else {
                    Ok(false)
                };
                if matches!(validation, Ok(true)) {
                    vms.insert(
                        vmid,
                        VMCacheData {
                            actor_ref: actor,
                            config: vm_config.clone(),
                            vcpus: vm_config.desired.compute.vcpus,
                            memory_bytes: vm_config.desired.compute.memory_bytes,
                            generation: placement.generation,
                        },
                    );
                } else {
                    warn!(%vmid, "Existing VM actor does not match recovered placement; reserving resources conservatively");
                    pending_resources.insert(
                        vmid,
                        StopResourceReservation {
                            generation: placement.generation,
                            vcpus: vm_config.desired.compute.vcpus,
                            memory_bytes: vm_config.desired.compute.memory_bytes,
                        },
                    );
                    pending_actors.insert(
                        vmid,
                        PendingStopActor {
                            actor_ref: actor,
                            generation: placement.generation,
                            owner: hostname.to_owned(),
                        },
                    );
                }
                continue;
            }
            let actor = VMActor::spawn_link(
                parent,
                (
                    vmid,
                    Some(vm_config.clone()),
                    placement.generation,
                    hostname.to_owned(),
                ),
            )
            .await;
            let startup = actor
                .wait_for_startup_with_result(|result| result.map_err(|error| error.to_string()))
                .await;
            if let Err(error) = startup {
                warn!(%error, %vmid, "Unable to start recovered VM actor");
                pending_resources.insert(
                    vmid,
                    StopResourceReservation {
                        generation: placement.generation,
                        vcpus: vm_config.desired.compute.vcpus,
                        memory_bytes: vm_config.desired.compute.memory_bytes,
                    },
                );
                pending_actors.insert(
                    vmid,
                    PendingStopActor {
                        actor_ref: actor,
                        generation: placement.generation,
                        owner: hostname.to_owned(),
                    },
                );
                continue;
            }
            let stop_fence = VMStopFence {
                generation: placement.generation,
                owner: hostname.to_owned(),
                lifecycle: PlacementLifecycle::Stopping,
            };
            match Self::validate_create_against(state_store, hostname, &create).await {
                Ok(true) => {}
                Ok(false) => {
                    if let Err(error) = Self::teardown_actor(&actor, vmid, stop_fence.clone()).await
                    {
                        warn!(%error, %vmid, "Recovered VM intent changed during startup; retaining reservation");
                        pending_resources.insert(
                            vmid,
                            StopResourceReservation {
                                generation: placement.generation,
                                vcpus: vm_config.desired.compute.vcpus,
                                memory_bytes: vm_config.desired.compute.memory_bytes,
                            },
                        );
                        pending_actors.insert(
                            vmid,
                            PendingStopActor {
                                actor_ref: actor,
                                generation: placement.generation,
                                owner: hostname.to_owned(),
                            },
                        );
                    }
                    continue;
                }
                Err(error) => {
                    warn!(%error, %vmid, "Unable to verify recovered VM intent after startup; retaining the running actor");
                    Self::register_vm_actor(&actor, vmid);
                    vms.insert(
                        vmid,
                        VMCacheData {
                            actor_ref: actor,
                            config: vm_config.clone(),
                            vcpus: vm_config.desired.compute.vcpus,
                            memory_bytes: vm_config.desired.compute.memory_bytes,
                            generation: placement.generation,
                        },
                    );
                    continue;
                }
            }
            Self::register_vm_actor(&actor, vmid);
            match Self::validate_create_against(state_store, hostname, &create).await {
                Ok(true) => {}
                Ok(false) => {
                    if let Err(error) = Self::teardown_actor(&actor, vmid, stop_fence).await {
                        warn!(%error, %vmid, "Recovered VM intent changed during registration; retaining reservation");
                        pending_resources.insert(
                            vmid,
                            StopResourceReservation {
                                generation: placement.generation,
                                vcpus: vm_config.desired.compute.vcpus,
                                memory_bytes: vm_config.desired.compute.memory_bytes,
                            },
                        );
                        pending_actors.insert(
                            vmid,
                            PendingStopActor {
                                actor_ref: actor,
                                generation: placement.generation,
                                owner: hostname.to_owned(),
                            },
                        );
                    }
                    continue;
                }
                Err(error) => {
                    warn!(%error, %vmid, "Unable to verify recovered VM intent after registration; retaining the running actor");
                    vms.insert(
                        vmid,
                        VMCacheData {
                            actor_ref: actor,
                            config: vm_config.clone(),
                            vcpus: vm_config.desired.compute.vcpus,
                            memory_bytes: vm_config.desired.compute.memory_bytes,
                            generation: placement.generation,
                        },
                    );
                    continue;
                }
            }
            vms.insert(
                vmid,
                VMCacheData {
                    actor_ref: actor,
                    config: vm_config.clone(),
                    vcpus: vm_config.desired.compute.vcpus,
                    memory_bytes: vm_config.desired.compute.memory_bytes,
                    generation: placement.generation,
                },
            );
        }
        (vms, pending_resources, pending_actors)
    }

    async fn create_with_cached_actor(
        &mut self,
        msg: &CreateVM,
        ctx: &Context<Self, CreateVMReply>,
    ) -> CreateVMReply {
        let vmid = msg.vmid;
        let Some((actor_ref, generation, config)) = self.vms.get(&vmid).map(|existing| {
            (
                existing.actor_ref.clone(),
                existing.generation,
                existing.config.clone(),
            )
        }) else {
            return self.spawn_vm(msg, ctx).await;
        };
        let heartbeat = tokio::time::timeout(
            Duration::from_secs(5),
            actor_ref.ask(crate::messages::vm::GetVMHeartbeat),
        )
        .await;
        let healthy = heartbeat.as_ref().is_ok_and(|result| {
            result
                .as_ref()
                .is_ok_and(|reply| reply.error.is_none() && reply.generation == msg.generation)
        });
        let validation = self.validate_create(msg).await;
        match Self::cached_actor_disposition(healthy, validation) {
            CachedActorDisposition::Keep => {
                if config == msg.config {
                    info!(?vmid, actor_id = ?actor_ref.id(), "VM already exists; treating create as idempotent");
                } else {
                    warn!(?vmid, actor_id = ?actor_ref.id(), "Ignoring conflicting create for existing VM");
                }
                if let Some(cache) = self.vms.get_mut(&vmid) {
                    cache.generation = msg.generation;
                }
                return CreateVMReply {
                    config: Some(config),
                    actor_id: Some(actor_ref.id().to_bytes()),
                };
            }
            CachedActorDisposition::PreserveUnverified(error) => {
                warn!(%error, %vmid, "Unable to verify VM durable intent; retaining it without teardown");
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
            CachedActorDisposition::PreserveMismatch => {
                warn!(%vmid, "Durable create intent changed; retaining the existing actor");
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
            CachedActorDisposition::Teardown => {}
        }
        let stale_generation = heartbeat
            .ok()
            .and_then(Result::ok)
            .map_or(generation, |reply| reply.generation);
        let fence = VMStopFence {
            generation: stale_generation,
            owner: self.config.get_hostname().to_owned(),
            lifecycle: PlacementLifecycle::Deleting,
        };
        match after_confirmed_teardown(Self::teardown_actor(&actor_ref, vmid, fence), || async {
            if self
                .vms
                .get(&vmid)
                .is_some_and(|current| current.actor_ref.id() == actor_ref.id())
            {
                self.remove_vm(vmid);
            }
            self.spawn_vm(msg, ctx).await
        })
        .await
        {
            Ok(reply) => reply,
            Err(error) => {
                warn!(%error, %vmid, "Unable to confirm teardown of failed VM actor; retaining it and refusing replacement");
                CreateVMReply {
                    config: None,
                    actor_id: None,
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn create_without_cached_actor(
        &mut self,
        msg: &CreateVM,
        ctx: &Context<Self, CreateVMReply>,
    ) -> CreateVMReply {
        let vmid = msg.vmid;
        let owner = self.config.get_hostname().to_owned();

        if self
            .pending_stop_actors
            .get(&vmid)
            .is_some_and(|pending| !pending.actor_ref.is_alive())
        {
            if !self
                .resolve_stopped_pending_actor(vmid, ctx.actor_ref())
                .await
            {
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
            return self.spawn_vm(msg, ctx).await;
        }

        let discovered_actor = if let Some(pending) = self.pending_stop_actors.get(&vmid) {
            Some(pending.actor_ref.clone())
        } else {
            Self::lookup_vm_actor(vmid).await
        };
        let Some(actor_ref) = discovered_actor else {
            return self.spawn_vm(msg, ctx).await;
        };
        if !self.pending_stop_resources.contains_key(&vmid) {
            self.reserve_stop_resources(
                vmid,
                msg.generation,
                msg.config.desired.compute.vcpus,
                msg.config.desired.compute.memory_bytes,
            );
        }
        let heartbeat = tokio::time::timeout(
            Duration::from_secs(5),
            actor_ref.ask(crate::messages::vm::GetVMHeartbeat),
        )
        .await;
        let Ok(Ok(reply)) = heartbeat else {
            let pending = PendingStopActor {
                actor_ref: actor_ref.clone(),
                generation: msg.generation,
                owner: owner.clone(),
            };
            self.pending_stop_actors.insert(vmid, pending);
            if !actor_ref.is_alive()
                && self
                    .resolve_stopped_pending_actor(vmid, ctx.actor_ref())
                    .await
            {
                return self.spawn_vm(msg, ctx).await;
            }
            warn!(%vmid, "Discovered VM actor is unresponsive; retaining reservation and refusing to overlap a replacement");
            return CreateVMReply {
                config: None,
                actor_id: None,
            };
        };

        if reply.error.is_none() && reply.generation == msg.generation {
            match self.validate_create(msg).await {
                Ok(true) => {
                    if self
                        .pending_stop_actors
                        .get(&vmid)
                        .is_some_and(|current| current.actor_ref.id() == actor_ref.id())
                    {
                        self.pending_stop_actors.remove(&vmid);
                    }
                    let actor_id = actor_ref.id().to_bytes();
                    self.insert_vm(
                        vmid,
                        VMCacheData {
                            actor_ref,
                            config: msg.config.clone(),
                            vcpus: msg.config.desired.compute.vcpus,
                            memory_bytes: msg.config.desired.compute.memory_bytes,
                            generation: msg.generation,
                        },
                    );
                    return CreateVMReply {
                        config: Some(msg.config.clone()),
                        actor_id: Some(actor_id),
                    };
                }
                Err(error) => {
                    warn!(%error, %vmid, "Unable to verify discovered VM intent; retaining the healthy actor and reservation");
                    self.pending_stop_actors.insert(
                        vmid,
                        PendingStopActor {
                            actor_ref,
                            generation: reply.generation,
                            owner,
                        },
                    );
                    return CreateVMReply {
                        config: None,
                        actor_id: None,
                    };
                }
                Ok(false) => {}
            }
        }

        match self.validate_create(msg).await {
            Ok(true) => {}
            Ok(false) => {
                warn!(%vmid, "Durable create intent changed before stale actor cleanup; retaining the actor");
                self.pending_stop_actors.insert(
                    vmid,
                    PendingStopActor {
                        actor_ref,
                        generation: reply.generation,
                        owner,
                    },
                );
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
            Err(error) => {
                warn!(%error, %vmid, "Unable to verify create intent before stale actor cleanup; retaining the actor");
                self.pending_stop_actors.insert(
                    vmid,
                    PendingStopActor {
                        actor_ref,
                        generation: reply.generation,
                        owner,
                    },
                );
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
        }
        let fence = VMStopFence {
            generation: reply.generation,
            owner: owner.clone(),
            lifecycle: PlacementLifecycle::Deleting,
        };
        match after_confirmed_teardown(Self::teardown_actor(&actor_ref, vmid, fence), || async {
            if self
                .pending_stop_actors
                .get(&vmid)
                .is_some_and(|current| current.actor_ref.id() == actor_ref.id())
            {
                self.pending_stop_actors.remove(&vmid);
            }
            self.release_stop_resources(vmid);
            self.spawn_vm(msg, ctx).await
        })
        .await
        {
            Ok(reply) => reply,
            Err(error) => {
                self.pending_stop_actors.insert(
                    vmid,
                    PendingStopActor {
                        actor_ref,
                        generation: reply.generation,
                        owner,
                    },
                );
                warn!(%error, %vmid, "Unable to clean up stale discovered VM actor; retaining reservation and refusing replacement");
                CreateVMReply {
                    config: None,
                    actor_id: None,
                }
            }
        }
    }

    fn register_vm_name<F, Fut, E>(
        name: String,
        actor_ref: WeakActorRef<VMActor>,
        mut register: F,
    ) -> tokio::task::JoinHandle<()>
    where
        F: FnMut(WeakActorRef<VMActor>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: std::fmt::Debug + Send + 'static,
    {
        let lock = registration_lock(&name);
        tokio::spawn(async move {
            let weak_for_continue = actor_ref.clone();
            run_ordered_registration(
                name,
                lock,
                move || {
                    weak_for_continue
                        .upgrade()
                        .is_some_and(|actor| actor.is_alive())
                },
                move || register(actor_ref.clone()),
            )
            .await;
        })
    }

    /// Serialize registrations by registry name. A pending registration for a
    /// dead actor finishes before a replacement can publish the same name.
    fn register_vm_actor(actor_ref: &ActorRef<VMActor>, vmid: Ulid) {
        let weak = actor_ref.downgrade();
        for name in [vm_actor_id(vmid), VM.to_owned()] {
            let _registration_task =
                Self::register_vm_name(name.clone(), weak.clone(), move |actor_ref| {
                    let name = name.clone();
                    async move {
                        let Some(actor_ref) = actor_ref.upgrade() else {
                            return Err("VM actor no longer exists".to_owned());
                        };
                        if !actor_ref.is_alive() {
                            return Err("VM actor is stopping".to_owned());
                        }
                        actor_ref
                            .register(name)
                            .await
                            .map_err(|error| error.to_string())
                    }
                });
        }
    }
}

#[allow(clippy::unused_async_trait_impl)]
impl Actor for AgentActor {
    type Args = (Config, Arc<StateStore>);
    type Error = Report;

    async fn on_start(
        (config, state_store): Self::Args,
        actor_ref: ActorRef<Self>,
    ) -> Result<Self> {
        let peer_id = *actor_ref.id().peer_id().unwrap();

        info!(?peer_id, "Agent Actor started!");

        // spawn networking actor
        let network_actor: ActorRef<NetworkAgentActor> =
            NetworkAgentActor::spawn_link(&actor_ref, config.network.clone()).await;
        network_actor.register(NETWORK).await?;

        let sys = System::new_all();
        let (manifest_values, placements) = state_store
            .list_vm_state()
            .await
            .map_err(|error| eyre!("unable to recover paired VM state: {error}"))?;
        let records = manifest_values
            .into_iter()
            .map(|(key, value)| {
                serde_json::from_value::<VmManifest>(value)
                    .map(|manifest| (key, manifest))
                    .map_err(|error| eyre!("unable to decode VM manifest snapshot: {error}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let (vms, pending_stop_resources, pending_stop_actors) = Self::recover_vms(
            &actor_ref,
            &state_store,
            config.get_hostname(),
            placements,
            records,
        )
        .await;
        let (used_vcpus, used_memory_bytes) =
            vms.values()
                .fold((0_u32, 0_u64), |(vcpus, memory_bytes), vm| {
                    (
                        vcpus.saturating_add(vm.vcpus),
                        memory_bytes.saturating_add(vm.memory_bytes),
                    )
                });
        let (pending_vcpus, pending_memory) = total_stop_reservations(&pending_stop_resources);
        let used_vcpus = used_vcpus.saturating_add(pending_vcpus);
        let used_memory_bytes = used_memory_bytes.saturating_add(pending_memory);
        info!(
            count = vms.len(),
            pending_stop_count = pending_stop_resources.len(),
            "Recovered VM manifests from cluster state"
        );

        Ok(Self {
            vcpus: u32::try_from(sys.cpus().len()).unwrap_or(u32::MAX),
            memory: ByteSize::b(sys.total_memory()),
            config,
            vms,
            pending_stop_resources,
            pending_stop_actors,
            used_vcpus,
            used_memory_bytes,
            membership_revision: 0,
            status_history: StatusChangeHistory::new(),
            metadata: ObjectMetadata::default(),
            state_store,
        })
    }

    async fn on_panic(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        err: PanicError,
    ) -> Result<ControlFlow<ActorStopReason>> {
        error!(
            ?err,
            "Agent actor panicked; stopping because its state cannot be safely rebuilt here"
        );
        Ok(ControlFlow::Break(ActorStopReason::Panicked(err)))
    }

    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>> {
        warn!("Linked actor {id:?} died with reason {reason:?}");

        let removed: Vec<_> = self
            .vms
            .iter()
            .filter(|(_, vm)| vm.actor_ref.id() == id)
            .map(|(vmid, vm)| {
                (
                    *vmid,
                    vm.actor_ref.clone(),
                    vm.generation,
                    vm.vcpus,
                    vm.memory_bytes,
                )
            })
            .collect();
        let mut retiring = Vec::with_capacity(removed.len());
        for (vmid, actor_ref, generation, vcpus, memory_bytes) in removed {
            // Transfer the existing charge to the stop reservation. If a newer
            // cleanup actor already owns this VM, keep that exact handle rather
            // than replacing it with this delayed link notification.
            self.reserve_stop_resources(vmid, generation, vcpus, memory_bytes);
            self.remove_vm(vmid);
            self.track_pending_stop_actor(
                vmid,
                actor_ref.clone(),
                generation,
                self.config.get_hostname().to_owned(),
            );
            retiring.push((
                vmid,
                PendingStopActor {
                    actor_ref,
                    generation,
                    owner: self.config.get_hostname().to_owned(),
                },
            ));
        }

        let additional_retiring: Vec<_> = self
            .pending_stop_actors
            .iter()
            .filter(|(_, pending)| pending.actor_ref.id() == id)
            .map(|(vmid, pending)| (*vmid, pending.clone()))
            .collect();
        for (vmid, pending) in additional_retiring {
            if !retiring
                .iter()
                .any(|(retiring_vmid, _)| *retiring_vmid == vmid)
            {
                retiring.push((vmid, pending));
            }
        }
        for (vmid, pending) in retiring {
            match Self::wait_for_shutdown_completion(
                pending.actor_ref.wait_for_shutdown_with_result(|result| {
                    result.cloned().map_err(|error| error.to_string())
                }),
            )
            .await
            {
                Ok(_) => {
                    if self
                        .pending_stop_actors
                        .get(&vmid)
                        .is_some_and(|current| current.actor_ref.id() == id)
                    {
                        self.pending_stop_actors.remove(&vmid);
                        self.release_stop_resources(vmid);
                    }
                }
                Err(ActorShutdownError::Failed(error)) => {
                    self.track_pending_stop_actor(
                        vmid,
                        pending.actor_ref,
                        pending.generation,
                        pending.owner,
                    );
                    warn!(%error, %vmid, "VM actor shutdown cleanup failed; retaining exact actor handle and resource reservation");
                }
                Err(ActorShutdownError::TimedOut) => {
                    self.track_pending_stop_actor(
                        vmid,
                        pending.actor_ref,
                        pending.generation,
                        pending.owner,
                    );
                    warn!(%vmid, "VM actor shutdown cleanup is still pending; retaining exact actor handle and resource reservation");
                }
            }
        }

        Ok(ControlFlow::Continue(()))
    }
}

#[remote_message]
impl Message<CreateVM> for AgentActor {
    type Reply = CreateVMReply;

    async fn handle(&mut self, msg: CreateVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let vmid = msg.vmid;
        match self.validate_create(&msg).await {
            Ok(true) => {}
            Ok(false) => {
                warn!(%vmid, generation = %msg.generation, "Rejecting create request that does not match authoritative active placement");
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
            Err(error) => {
                warn!(%error, %vmid, "Unable to verify authoritative create intent; rejecting without runtime cleanup");
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                };
            }
        }
        if self.vms.contains_key(&vmid) {
            return self.create_with_cached_actor(&msg, ctx).await;
        }
        self.create_without_cached_actor(&msg, ctx).await
    }
}

#[remote_message]
impl Message<MigrateVMReceive> for AgentActor {
    type Reply = MigrateVMReceiveReply;

    async fn handle(
        &mut self,
        msg: MigrateVMReceive,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vmid = msg.vmid;
        let owner = self.config.get_hostname().to_owned();
        let generation = self
            .state_store
            .get::<PlacementRecord>(&key(PLACEMENT_PREFIX, &vmid))
            .await
            .ok()
            .flatten()
            .filter(|placement| placement.node == owner)
            .map_or_else(Ulid::nil, |placement| placement.generation);
        let actor_ref = VMActor::spawn_link(ctx.actor_ref(), (vmid, None, generation, owner)).await;

        Self::register_vm_actor(&actor_ref, vmid);
        self.insert_vm(
            vmid,
            VMCacheData {
                actor_ref: actor_ref.clone(),
                config: msg.config.clone(),
                vcpus: msg.config.desired.compute.vcpus,
                memory_bytes: msg.config.desired.compute.memory_bytes,
                generation,
            },
        );

        let reply = match actor_ref.ask(msg).await {
            Ok(reply) => reply,
            Err(error) => MigrateVMReceiveReply {
                listening_address: String::new(),
                error: Some(format!("failed to start migration receiver: {error}")),
            },
        };

        if reply.error.is_some() {
            if self
                .vms
                .get(&vmid)
                .is_some_and(|cache| cache.actor_ref.id() == actor_ref.id())
            {
                self.remove_vm(vmid);
            }
            actor_ref.kill();
        }

        reply
    }
}

#[remote_message]
impl Message<DeleteVM> for AgentActor {
    type Reply = DeleteVMReply;

    async fn handle(&mut self, msg: DeleteVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        match self
            .stop_runtime(msg.vmid, msg.expected, ctx.actor_ref())
            .await
        {
            Ok(completed) => DeleteVMReply {
                error: None,
                completed: Some(completed),
            },
            Err(error) => {
                warn!(vm_id = %msg.vmid, %error, "failed to complete durable VM deletion");
                DeleteVMReply {
                    error: Some(error),
                    completed: None,
                }
            }
        }
    }
}

#[remote_message]
impl Message<ShutdownVM> for AgentActor {
    type Reply = Result<ShutdownVMReply, String>;

    async fn handle(
        &mut self,
        msg: ShutdownVM,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        trace!(?msg, "Requesting durable VM shutdown");
        self.stop_runtime(msg.vmid, msg.expected, ctx.actor_ref())
            .await
            .map(|completed| ShutdownVMReply { completed })
    }
}
// forward GetVMInfo to VM actor
#[remote_message]
impl Message<GetVMInfo> for AgentActor {
    type Reply = ForwardedReply<GetVMInfo, GetVMInfoReply>;

    async fn handle(
        &mut self,
        msg: GetVMInfo,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let Some(vmid) = msg.vmid else {
            warn!("No vmid provided for Agent Actor GetVMInfo forwarding");
            return ForwardedReply::from_ok(GetVMInfoReply {
                vmid: Ulid::nil(),
                config: None,
            });
        };

        let Some(actor_ref) = Self::lookup_vm_actor(vmid).await else {
            warn!(vm_id = %vmid, "VM actor not found for info lookup");
            return ForwardedReply::from_ok(GetVMInfoReply { vmid, config: None });
        };

        ctx.forward(&actor_ref, msg).await
    }
}

#[remote_message]
#[allow(clippy::unused_async_trait_impl)]
impl Message<AgentListVMs> for AgentActor {
    type Reply = AgentListVMsReply;

    async fn handle(
        &mut self,
        _msg: AgentListVMs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // look up with cache
        let vms = self.vms.keys().copied().collect();
        // let vms_actors: Vec<_> = RemoteActorRef::<VMActor>::lookup_all("vm").collect().await;

        // let mut vms = Vec::new();
        // for actor in vms_actors.into_iter().flatten() {
        //     trace!(?actor, "looking up VM info");
        //     if let Ok(reply) = actor.ask(&GetVMInfo).await {
        //         vms.push(reply.vmid);
        //     }
        // }

        AgentListVMsReply { vms }
    }
}

#[remote_message]
#[allow(clippy::unused_async_trait_impl)]
impl Message<Ping> for AgentActor {
    type Reply = Pong;

    async fn handle(&mut self, _msg: Ping, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        Pong
    }
}

#[remote_message]
impl Message<PanicAgent> for AgentActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: PanicAgent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        tracing::info!("panicking");
        panic!();
    }
}

#[remote_message]
#[allow(clippy::unused_async_trait_impl)]
impl Message<GetAgentStatus> for AgentActor {
    type Reply = AgentStatusUpdate;

    async fn handle(
        &mut self,
        msg: GetAgentStatus,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let full_status = self.status_snapshot();
        let used_vcpus = full_status.used_vcpus;
        let used_ram = full_status.used_ram;

        if msg.initial
            || msg.since_revision > self.membership_revision
            || self
                .status_history
                .front()
                .is_some_and(|change| msg.since_revision.saturating_add(1) < change.revision)
        {
            return AgentStatusUpdate::Full {
                revision: self.membership_revision,
                status: full_status,
            };
        }

        let mut latest_changes = AHashMap::new();
        for change in self
            .status_history
            .iter()
            .filter(|change| change.revision > msg.since_revision)
        {
            latest_changes.insert(change.vmid, change.added);
        }
        let mut added = Vec::with_capacity(latest_changes.len());
        let mut removed = Vec::with_capacity(latest_changes.len());
        for (vmid, added_change) in latest_changes {
            if added_change {
                added.push(vmid);
            } else {
                removed.push(vmid);
            }
        }
        added.sort_unstable();
        removed.sort_unstable();
        AgentStatusUpdate::Delta {
            revision: self.membership_revision,
            added,
            removed,
            used_vcpus,
            used_ram,
            reserved_vms: full_status.reserved_vms,
            resource_charges: full_status.resource_charges,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActorId, AgentActor, CachedActorDisposition, StopResourceReservation,
        after_confirmed_teardown, cached_predecessor_reservation, can_track_pending_stop_actor,
        failed_stop_reservation, registration_lock, run_ordered_registration,
        stop_fence_matches_placement, total_stop_reservations,
    };
    use crate::{
        config::Config,
        manifest::{Boot, Compute, DesiredState, MANIFEST_VERSION, Metadata, VmManifest},
        messages::{agent::VMResourceCharge, vm::CreateVM},
    };
    use ahash::AHashMap;
    use bytesize::ByteSize;
    use odorobo::cluster_state::{
        MemoryStateStore, PlacementLifecycle, PlacementRecord, StateStore,
    };
    use std::{sync::Arc, time::Duration};
    use tokio::sync::{Mutex, oneshot};
    use ulid::Ulid;

    fn manifest(id: Ulid, vcpus: u32, memory_bytes: u64) -> VmManifest {
        VmManifest {
            api_version: MANIFEST_VERSION,
            id,
            desired: DesiredState {
                metadata: Metadata {
                    name: id.to_string(),
                    ..Default::default()
                },
                compute: Compute {
                    vcpus,
                    memory_bytes,
                    ..Default::default()
                },
                boot: Boot::default(),
                ..Default::default()
            },
            observed: None,
        }
    }

    fn test_agent() -> AgentActor {
        AgentActor {
            vcpus: 32,
            memory: ByteSize::b(1024),
            used_vcpus: 0,
            used_memory_bytes: 0,
            membership_revision: 0,
            status_history: std::collections::VecDeque::default(),
            config: Config {
                hostname: Some("node-a".to_owned()),
                ..Default::default()
            },
            vms: AHashMap::new(),
            pending_stop_resources: AHashMap::new(),
            pending_stop_actors: AHashMap::new(),
            metadata: crate::types::ObjectMetadata::default(),
            state_store: Arc::new(StateStore::Memory(MemoryStateStore::default())),
        }
    }

    #[tokio::test]
    async fn delayed_actor_registration_future_is_not_cancelled_by_a_timeout() {
        let (sender, receiver) = oneshot::channel::<()>();
        let receiver = Arc::new(Mutex::new(Some(receiver)));
        let task_receiver = Arc::clone(&receiver);
        let mut registration = tokio::spawn(run_ordered_registration(
            "vm:delayed".to_owned(),
            registration_lock("vm:delayed"),
            || true,
            move || {
                let receiver = Arc::clone(&task_receiver);
                async move {
                    receiver
                        .lock()
                        .await
                        .take()
                        .expect("registration future is only polled once")
                        .await
                        .map_err(|error| error.to_string())
                }
            },
        ));

        tokio::time::timeout(Duration::from_millis(10), &mut registration)
            .await
            .unwrap_err();
        sender.send(()).expect("complete delayed registration");
        registration.await.expect("registration task should finish");
    }

    #[tokio::test]
    async fn blocked_teardown_does_not_start_replacement() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (release_tx, release_rx) = oneshot::channel::<()>();
        let replacement_started = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&replacement_started);
        let task = tokio::spawn(after_confirmed_teardown(
            async move {
                release_rx.await.map_err(|error| error.to_string())?;
                Ok::<(), String>(())
            },
            move || async move {
                started.store(true, Ordering::SeqCst);
                "replacement started"
            },
        ));

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!replacement_started.load(Ordering::SeqCst));
        release_tx.send(()).expect("release blocked teardown");
        assert_eq!(task.await.unwrap().unwrap(), "replacement started");
        assert!(replacement_started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn link_notification_does_not_release_vm_until_on_stop_completes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (cleanup_tx, cleanup_rx) = oneshot::channel::<()>();
        let replacement_started = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&replacement_started);
        let shutdown = AgentActor::wait_for_shutdown_completion(async move {
            cleanup_rx.await.map_err(|error| error.to_string())?;
            Ok::<(), String>(())
        });
        let task = tokio::spawn(after_confirmed_teardown(shutdown, move || async move {
            started.store(true, Ordering::SeqCst);
            "replacement"
        }));

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!replacement_started.load(Ordering::SeqCst));
        cleanup_tx
            .send(())
            .expect("complete delayed on_stop cleanup");
        assert_eq!(task.await.unwrap().unwrap(), "replacement");
        assert!(replacement_started.load(Ordering::SeqCst));
    }

    #[test]
    fn pending_actorless_allocation_transfers_once_and_delete_releases_it() {
        let vmid = Ulid::generate();
        let mut agent = test_agent();
        agent.reserve_stop_resources(vmid, Ulid::generate(), 3, 4096);
        assert_eq!(agent.used_vcpus, 3);
        assert_eq!(agent.used_memory_bytes, 4096);
        assert_eq!(agent.membership_revision, 1);

        // Successful create/adoption moves the existing reservation into the
        // running tally rather than charging the same allocation twice.
        assert!(agent.charge_running_resources(vmid, 3, 4096));
        assert!(!agent.pending_stop_resources.contains_key(&vmid));
        assert_eq!((agent.used_vcpus, agent.used_memory_bytes), (3, 4096));

        // A later delete removes the running charge; a stale reservation release
        // is idempotent and leaves capacity at zero.
        agent.release_running_resources(3, 4096);
        agent.release_stop_resources(vmid);
        assert_eq!((agent.used_vcpus, agent.used_memory_bytes), (0, 0));
    }

    #[test]
    fn first_full_status_reports_actorless_vm_capacity_reservations() {
        let vmid = Ulid::generate();
        let generation = Ulid::generate();
        let mut agent = test_agent();
        agent.reserve_stop_resources(vmid, generation, 5, 8192);

        let status = agent.status_snapshot();

        assert!(status.vms.is_empty());
        assert_eq!(status.reserved_vms, vec![vmid]);
        assert_eq!(
            status.used_vcpus,
            5_u32.saturating_add(agent.config.get_reserved_vcpus())
        );
        assert_eq!(status.used_ram, ByteSize::b(8192));
        assert_eq!(
            status.resource_charges,
            vec![VMResourceCharge {
                vmid,
                generation,
                vcpus: 5,
                memory_bytes: 8192,
            }]
        );
    }

    #[test]
    fn reservation_only_status_changes_advance_the_revision() {
        let vmid = Ulid::generate();
        let mut agent = test_agent();
        agent.reserve_stop_resources(vmid, Ulid::generate(), 2, 2048);
        let reserved_revision = agent.membership_revision;
        agent.release_stop_resources(vmid);
        assert_eq!(reserved_revision, 1);
        assert_eq!(agent.membership_revision, reserved_revision + 1);
    }

    #[tokio::test]
    async fn delayed_dead_actor_registration_cannot_overwrite_replacement() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let name = format!("vm:registration-race:{}", Ulid::generate());
        let state = Arc::new(Mutex::new(None::<&'static str>));
        let actor_a_alive = Arc::new(AtomicBool::new(true));
        let (started_tx, started_rx) = oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let (release_tx, release_rx) = oneshot::channel();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let a_state = Arc::clone(&state);
        let a_alive = Arc::clone(&actor_a_alive);
        let a_release = Arc::clone(&release_rx);
        let name_lock = registration_lock(&name);
        let actor_a = tokio::spawn(run_ordered_registration(
            name.clone(),
            Arc::clone(&name_lock),
            move || a_alive.load(Ordering::SeqCst),
            move || {
                let state = Arc::clone(&a_state);
                let release = Arc::clone(&a_release);
                let started = Arc::clone(&started_tx);
                async move {
                    let sender = started.lock().await.take();
                    if let Some(sender) = sender {
                        sender.send(()).ok();
                    }
                    release
                        .lock()
                        .await
                        .take()
                        .expect("registration only begins once")
                        .await
                        .expect("release A registration");
                    *state.lock().await = Some("A");
                    Ok::<(), ()>(())
                }
            },
        ));
        started_rx.await.expect("A registration started");
        actor_a_alive.store(false, Ordering::SeqCst);

        let b_state = Arc::clone(&state);
        let actor_b = tokio::spawn(run_ordered_registration(
            name.clone(),
            name_lock,
            || true,
            move || {
                let state = Arc::clone(&b_state);
                async move {
                    *state.lock().await = Some("B");
                    Ok::<(), ()>(())
                }
            },
        ));
        tokio::task::yield_now().await;
        release_tx.send(()).expect("release delayed A registration");
        actor_a.await.expect("A registration worker finishes");
        actor_b.await.expect("B registration worker finishes");
        assert_eq!(*state.lock().await, Some("B"));
    }

    #[test]
    fn delayed_old_actor_notification_preserves_pending_recovery_owner() {
        let actor_a = ActorId::new(1);
        let actor_b = ActorId::new(2);
        let mut pending_actor = Some(actor_b);

        // B's cleanup timed out and remains the exact pending handle. A's queued
        // link notification must not replace it with the older failed actor.
        if can_track_pending_stop_actor(pending_actor, actor_a) {
            pending_actor = Some(actor_a);
        }
        assert_eq!(pending_actor, Some(actor_b));
    }

    #[test]
    fn failed_predecessor_notification_cannot_restore_charge_after_cleanup_handoff() {
        let vmid = Ulid::generate();
        let generation_a = Ulid::generate();
        let generation_g2 = Ulid::generate();
        let actor_a = ActorId::new(1);
        let actor_b = ActorId::new(2);
        let old_charge = StopResourceReservation {
            generation: generation_a,
            vcpus: 4,
            memory_bytes: 4 * 1024 * 1024 * 1024,
        };
        let mut cached = AHashMap::from([(vmid, (actor_a, old_charge))]);
        let mut pending = AHashMap::from([(vmid, actor_a)]);
        let mut reservations = AHashMap::new();

        // Cleanup hands off from A to B. Retiring A moves the existing charge
        // into the single pending reservation and removes A from the live cache.
        pending.insert(vmid, actor_b);
        if let Some((cached_actor, charge)) = cached.get(&vmid).copied()
            && let Some(reservation) = cached_predecessor_reservation(
                cached_actor,
                charge.generation,
                charge.vcpus,
                charge.memory_bytes,
                actor_b,
                generation_a,
            )
        {
            cached.remove(&vmid);
            reservations.insert(vmid, reservation);
        }
        assert!(!cached.contains_key(&vmid));
        assert_eq!(pending.get(&vmid), Some(&actor_b));
        assert_eq!(
            total_stop_reservations(&reservations),
            (4, 4 * 1024 * 1024 * 1024)
        );

        // A failed B cleanup attempt retains the exact handle and reservation;
        // its successful retry then releases both.
        assert_eq!(pending.get(&vmid), Some(&actor_b));
        pending.remove(&vmid);
        reservations.remove(&vmid);

        // A's delayed notification has no live cached entry to turn back into a
        // reservation or pending handle.
        if let Some((cached_actor, charge)) = cached.get(&vmid).copied()
            && cached_actor == actor_a
        {
            reservations.insert(vmid, charge);
            pending.insert(vmid, actor_a);
        }
        assert!(cached.is_empty());
        assert!(pending.is_empty());
        assert_eq!(total_stop_reservations(&reservations), (0, 0));

        // The new durable incarnation is not fenced by A's completed stop and can
        // reclaim capacity with its larger allocation.
        let placement_g2 = PlacementRecord {
            vmid,
            node: "node-a".to_owned(),
            generation: generation_g2,
            lifecycle: PlacementLifecycle::Active,
        };
        let mut stopping_a = placement_g2.clone();
        stopping_a.generation = generation_a;
        stopping_a.lifecycle = PlacementLifecycle::Deleting;
        assert!(!stop_fence_matches_placement(
            &placement_g2,
            &stopping_a.stop_fence(),
            "node-a"
        ));
        cached.insert(
            vmid,
            (
                actor_b,
                StopResourceReservation {
                    generation: generation_g2,
                    vcpus: 4,
                    memory_bytes: 24 * 1024 * 1024 * 1024,
                },
            ),
        );
        assert_eq!(cached[&vmid].1.generation, generation_g2);
        assert_eq!(cached[&vmid].1.memory_bytes, 24 * 1024 * 1024 * 1024);
    }

    #[test]
    fn failed_startup_stop_keeps_vm_resources_reserved() {
        let vmid = Ulid::generate();
        let mut stopping = PlacementRecord::active(vmid, "node-a".to_owned());
        stopping.lifecycle = PlacementLifecycle::Deleting;
        let manifest = manifest(vmid, 6, 8 * 1024 * 1024 * 1024);
        let resources = failed_stop_reservation(&stopping, &stopping, &manifest, "node-a")
            .expect("unconfirmed stop must keep its allocation reserved");
        let pending = AHashMap::from([(vmid, resources)]);
        assert_eq!(
            total_stop_reservations(&pending),
            (6, 8 * 1024 * 1024 * 1024)
        );
        assert_eq!(pending[&vmid].generation, stopping.generation);

        let next_generation = PlacementRecord::active(vmid, "node-a".to_owned());
        assert!(
            failed_stop_reservation(&stopping, &next_generation, &manifest, "node-a").is_none()
        );
    }

    #[test]
    fn delayed_stop_fence_cannot_target_a_recreated_vm() {
        let vmid = Ulid::generate();
        let mut old_stopping = PlacementRecord::active(vmid, "node-a".to_owned());
        old_stopping.lifecycle = PlacementLifecycle::Deleting;
        let old_fence = old_stopping.stop_fence();
        let new = PlacementRecord::active(vmid, "node-a".to_owned());
        assert_ne!(old_fence.generation, new.generation);
        assert!(!stop_fence_matches_placement(&new, &old_fence, "node-a"));
        assert!(!stop_fence_matches_placement(
            &old_stopping,
            &old_fence,
            "node-b"
        ));
        assert!(stop_fence_matches_placement(
            &old_stopping,
            &old_fence,
            "node-a"
        ));
    }

    #[test]
    fn transient_store_failure_after_healthy_heartbeat_preserves_cached_actor() {
        assert!(matches!(
            AgentActor::cached_actor_disposition(true, Err("temporary backend outage".to_owned())),
            CachedActorDisposition::PreserveUnverified(_)
        ));
        assert!(matches!(
            AgentActor::cached_actor_disposition(true, Ok(false)),
            CachedActorDisposition::PreserveMismatch
        ));
        assert!(matches!(
            AgentActor::cached_actor_disposition(false, Ok(true)),
            CachedActorDisposition::Teardown
        ));
    }

    #[test]
    fn stale_or_wrong_owner_create_commands_are_rejected() {
        let vmid = Ulid::generate();
        let config = manifest(vmid, 2, 512);
        let placement = PlacementRecord::active(vmid, "node-a".to_owned());
        let command = CreateVM {
            vmid,
            generation: placement.generation,
            config: config.clone(),
        };
        assert!(AgentActor::create_matches_authoritative_state(
            Some(placement.clone()),
            Some(config.clone()),
            "node-a",
            &command,
        ));

        let mut stopping = placement.clone();
        stopping.lifecycle = PlacementLifecycle::Deleting;
        assert!(!AgentActor::create_matches_authoritative_state(
            Some(stopping),
            Some(config.clone()),
            "node-a",
            &command,
        ));
        assert!(!AgentActor::create_matches_authoritative_state(
            Some(placement.clone()),
            Some(config.clone()),
            "node-b",
            &command,
        ));
        let mut obsolete = command;
        obsolete.generation = Ulid::generate();
        assert!(!AgentActor::create_matches_authoritative_state(
            Some(placement),
            Some(config),
            "node-a",
            &obsolete,
        ));
    }

    #[test]
    fn recovery_selects_local_manifests_and_restores_resource_usage() {
        let local_id = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ULID");
        let remote_id = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAW").expect("valid ULID");
        let orphan_id = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAX").expect("valid ULID");
        let stopping_id = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAY").expect("valid ULID");
        let mut stopping_placement = PlacementRecord::active(stopping_id, "node-a".to_owned());
        stopping_placement.lifecycle = PlacementLifecycle::Stopping;
        let recovered = AgentActor::manifests_for_node(
            vec![
                (
                    "placement/local".to_owned(),
                    PlacementRecord::active(local_id, "node-a".to_owned()),
                ),
                (
                    "placement/remote".to_owned(),
                    PlacementRecord::active(remote_id, "node-b".to_owned()),
                ),
                ("placement/stopping".to_owned(), stopping_placement),
            ],
            vec![
                ("manifest/local".to_owned(), manifest(local_id, 2, 512)),
                ("manifest/remote".to_owned(), manifest(remote_id, 4, 1024)),
                ("manifest/orphan".to_owned(), manifest(orphan_id, 8, 2048)),
                (
                    "manifest/stopping".to_owned(),
                    manifest(stopping_id, 16, 4096),
                ),
            ],
            "node-a",
        );

        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].id, local_id);
        assert_eq!(AgentActor::recovered_resources(&recovered), (2, 512));
    }
}
