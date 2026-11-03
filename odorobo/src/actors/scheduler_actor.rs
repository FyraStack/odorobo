//! The scheduler actor and its in-memory cluster view.
//!
//! `SchedulerActor` makes placement decisions from periodically refreshed agent
//! status rather than issuing network requests while scoring. Its state is
//! eventually consistent and is not a durable source of truth:
//!
//! - [`SchedulerActor::vm_manifests`] records VM intent.
//! - [`SchedulerActor::vm_placements`] records desired and observed placements.
//!   A VM can have more than one entry while it is migrating.
//! - [`SchedulerActor::vm_data_cache`] tracks discovered VM actors. A `None`
//!   actor reference is a deliberate unresolved-placement placeholder.
//! - [`SchedulerActor::agent_vm_index`] is an observation index, not placement
//!   authority.
//!
//! Implementation is split by responsibility into cache reconciliation,
//! discovery/polling, scheduling policy, and actor message handlers.

mod cache;
mod discovery;
mod handlers;
mod scheduling;

#[cfg(test)]
mod tests;

use std::time::Instant;

use ahash::{AHashMap, AHashSet};
use kameo::prelude::*;
use tokio::task::JoinHandle;
use ulid::Ulid;

use crate::actors::agent_actor::AgentActor;
use crate::ch_driver::actor::VMActor;
use crate::manifest::VmManifest;
use crate::messages::agent::{AgentStatus, AgentStatusUpdate};
use crate::messages::vm::GetVMInfoReply;

/// Internal discovery event that starts VM polling for a newly found actor.
#[derive(Debug)]
struct VmActorDiscovered {
    actor_ref: RemoteActorRef<VMActor>,
}

/// Internal discovery event that starts status polling for a newly found agent.
#[derive(Debug)]
struct AgentActorDiscovered {
    actor_ref: RemoteActorRef<AgentActor>,
}

/// The cache domain that owns cleanup for a linked remote actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CachedActorKind {
    Agent,
    Vm,
}

/// A VM's initial identity and configuration snapshot from its updater task.
#[derive(Debug)]
struct VmUpdated {
    actor_ref: RemoteActorRef<VMActor>,
    data: GetVMInfoReply,
}

/// Notification that a VM updater exceeded its reachability-failure budget.
#[derive(Debug)]
struct VmUpdaterStopped {
    actor_id: ActorId,
}

/// A revisioned agent-status update forwarded by its updater task.
#[derive(Debug)]
struct AgentUpdated {
    actor_id: ActorId,
    actor_ref: RemoteActorRef<AgentActor>,
    update: AgentStatusUpdate,
}

/// Notification that an agent updater exceeded its reachability-failure budget.
#[derive(Debug)]
struct AgentUpdaterStopped {
    actor_id: ActorId,
}

/// Periodic maintenance trigger for expiring unresolved VM placements.
#[derive(Debug)]
struct ReconcileVmPlacements;

/// The latest scheduler-approved status snapshot for an agent.
///
/// The first accepted update must be a full snapshot. Later updates are
/// revision-ordered; stale or duplicate revisions are ignored.
#[derive(Debug, Clone)]
pub struct CachedAgentActor {
    pub actor_ref: RemoteActorRef<AgentActor>,
    pub data: AgentStatus,
    pub status_revision: u64,
}

/// Scheduler-side lifecycle for a VM placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmLifecycle {
    /// Resources are reserved after create dispatch but before the destination
    /// agent has reported the VM. This does not prove the VM process exists.
    Pending,
    /// The destination agent has reported the VM in its status.
    Running,
}

/// A desired or observed placement of a VM on an agent.
///
/// Multiple entries for one VM are valid during migration. `last_confirmed_at`
/// is updated only from agent status; `created_at` is used to expire unresolved
/// pending placeholders.
#[derive(Debug, Clone)]
pub struct VmPlacement {
    pub agent_id: ActorId,
    pub lifecycle: VmLifecycle,
    pub created_at: Instant,
    pub last_confirmed_at: Option<Instant>,
}

/// A discovered VM actor associated with one concurrent placement.
///
/// `actor_ref: None` intentionally represents a placement reserved before its
/// VM actor is discovered.
#[derive(Debug, Clone)]
pub struct CachedVMActor {
    pub actor_ref: Option<RemoteActorRef<VMActor>>,
}

/// An eventually consistent, in-memory VM scheduler.
///
/// Public caches are exposed for inspection, but correlated maps must be
/// updated together through the actor's message handlers to preserve placement
/// and pending-resource accounting invariants.
#[derive(RemoteActor)]
pub struct SchedulerActor {
    /// Latest status snapshot for each known agent, used for scheduling decisions.
    pub agent_data_cache: AHashMap<ActorId, CachedAgentActor>,
    /// Polling tasks that refresh corresponding agent cache entries.
    pub agent_keepalive_tasks: AHashMap<ActorId, JoinHandle<()>>,
    /// Maps discovered VM actor IDs to canonical VM IDs.
    pub vm_actorid_ulid_map: AHashMap<ActorId, Ulid>,
    /// Canonical VM intent retained while a VM is reconciled or migrated.
    pub vm_manifests: AHashMap<Ulid, VmManifest>,
    /// Desired and observed VM placements; multiple entries allow migration.
    pub vm_placements: AHashMap<Ulid, Vec<VmPlacement>>,
    /// VM actor references. `None` marks a placement awaiting discovery.
    pub vm_data_cache: AHashMap<Ulid, Vec<CachedVMActor>>,
    /// Polling tasks that refresh corresponding VM actor cache entries.
    pub vm_keepalive_tasks: AHashMap<ActorId, JoinHandle<()>>,
    /// Lazily computed resources reserved by `Pending` placements, keyed by agent.
    /// Invalidated whenever status or placement state that affects capacity changes.
    pending_resources_cache: Option<AHashMap<ActorId, (u32, u64)>>,
    /// Status-derived VM membership index used to evaluate VM affinity efficiently.
    /// This is an optimization and must never override `vm_placements` intent.
    agent_vm_index: AHashMap<ActorId, AHashSet<Ulid>>,
    /// Classifies linked actors so link-death cleanup affects the owning cache only.
    actor_kinds: AHashMap<ActorId, CachedActorKind>,
    /// Background discovery and reconciliation task.
    pub cache_actor_finder: Option<JoinHandle<()>>,
}
