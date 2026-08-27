//! Cache maintenance, placement reconciliation, and actor cleanup.

use std::time::{Duration, Instant};

use crate::messages::agent::AgentStatus;
use ahash::{AHashMap, AHashSet};
use kameo::prelude::*;
use tracing::trace;
use ulid::Ulid;

use crate::actors::agent_actor::AgentActor;
use crate::manifest::VmManifest;

use super::{CachedVMActor, SchedulerActor, VmLifecycle, VmPlacement};

const UNRESOLVED_VM_CACHE_TIMEOUT: Duration = Duration::from_secs(30);
const UNCONFIRMED_RUNNING_PLACEMENT_TIMEOUT: Duration = Duration::from_secs(30);

impl SchedulerActor {
    /// Releases excess allocation once an entry list no longer represents migration.
    pub(super) fn shrink_non_migrating_entries<T>(entries: &mut Vec<T>) {
        if entries.len() <= 1 && entries.capacity() >= 4 {
            entries.shrink_to_fit();
        }
    }

    /// Replaces a matching discovered VM entry, fulfills one unresolved placeholder,
    /// or records an additional entry for a concurrent migration placement.
    ///
    /// Matching is ordered by actor ID, then the first `None` placeholder, then
    /// append. Placement and cache vectors are not positionally correlated.
    pub(super) fn update_cached_vm_entry(
        entries: &mut Vec<CachedVMActor>,
        actor_id: ActorId,
        cached_vm: CachedVMActor,
    ) {
        if let Some(entry) = entries.iter_mut().find(|entry| {
            entry
                .actor_ref
                .as_ref()
                .is_some_and(|actor| actor.id() == actor_id)
        }) {
            *entry = cached_vm;
        } else if let Some(entry) = entries.iter_mut().find(|entry| entry.actor_ref.is_none()) {
            *entry = cached_vm;
        } else {
            entries.push(cached_vm);
        }
        Self::shrink_non_migrating_entries(entries);
    }

    /// Expires pending placements that were never confirmed by agent status.
    ///
    /// Pending entries expire after 30 seconds. The five-second discovery loop
    /// triggers this maintenance. Expiry removes only the unconfirmed placement:
    /// the manifest remains scheduler intent, allowing periodic reconciliation to
    /// dispatch a replacement create request.
    pub(super) fn cleanup_unresolved_vm_cache(
        _manifests: &mut AHashMap<Ulid, VmManifest>,
        placements: &mut AHashMap<Ulid, Vec<VmPlacement>>,
        data_cache: &mut AHashMap<Ulid, Vec<CachedVMActor>>,
    ) {
        let now = Instant::now();
        let empty_vmids: Vec<_> = placements
            .iter_mut()
            .filter_map(|(vmid, entries)| {
                entries.retain(|entry| {
                    entry.lifecycle != VmLifecycle::Pending
                        || now.duration_since(entry.created_at) < UNRESOLVED_VM_CACHE_TIMEOUT
                });
                Self::shrink_non_migrating_entries(entries);
                entries.is_empty().then_some(*vmid)
            })
            .collect();

        for vmid in empty_vmids {
            if data_cache
                .get(&vmid)
                .is_some_and(|entries| entries.iter().all(|entry| entry.actor_ref.is_none()))
            {
                data_cache.remove(&vmid);
            }
        }

        // Keep manifests and empty placement entries: they represent desired VM
        // state and are consumed by `ReconcileVmPlacements`.
    }

    /// Returns every VM that could be on an agent, deduplicating each source in
    /// constant expected time.
    ///
    /// The union includes current status, the status-derived index, and desired
    /// placements to avoid affinity decisions missing in-flight changes. Output is
    /// observation-first, then hash-map iteration order; callers must not rely on it.
    pub(super) fn placement_vm_ids(
        placements: &AHashMap<Ulid, Vec<VmPlacement>>,
        indexed: Option<&AHashSet<Ulid>>,
        agent_id: ActorId,
        observed: &[Ulid],
    ) -> Vec<Ulid> {
        let indexed_len = indexed.map_or(0, |index| index.len());
        let mut vmids = Vec::with_capacity(observed.len().max(indexed_len));
        let mut seen = AHashSet::with_capacity(observed.len().saturating_add(indexed_len));

        for vmid in observed {
            if seen.insert(*vmid) {
                vmids.push(*vmid);
            }
        }
        if let Some(indexed) = indexed {
            for vmid in indexed {
                if seen.insert(*vmid) {
                    vmids.push(*vmid);
                }
            }
        }
        for vmid in placements.iter().filter_map(|(vmid, entries)| {
            entries
                .iter()
                .any(|entry| entry.agent_id == agent_id)
                .then_some(vmid)
        }) {
            if seen.insert(*vmid) {
                vmids.push(*vmid);
            }
        }
        vmids
    }

    /// Removes all scheduler state correlated with a VM identifier.
    pub(super) fn remove_vm_state(
        vmid: Ulid,
        manifests: &mut AHashMap<Ulid, VmManifest>,
        placements: &mut AHashMap<Ulid, Vec<VmPlacement>>,
        data_cache: &mut AHashMap<Ulid, Vec<CachedVMActor>>,
    ) {
        manifests.remove(&vmid);
        placements.remove(&vmid);
        data_cache.remove(&vmid);
    }

    /// Removes a departed actor from every VM cache entry without altering placement intent.
    pub(super) fn remove_vm_actor(
        actor_id: ActorId,
        data_cache: &mut AHashMap<Ulid, Vec<CachedVMActor>>,
    ) {
        let empty_vmids: Vec<_> = data_cache
            .iter_mut()
            .filter_map(|(vmid, entries)| {
                entries.retain(|entry| {
                    entry
                        .actor_ref
                        .as_ref()
                        .is_none_or(|actor| actor.id() != actor_id)
                });
                Self::shrink_non_migrating_entries(entries);
                entries.is_empty().then_some(*vmid)
            })
            .collect();
        for vmid in empty_vmids {
            data_cache.remove(&vmid);
        }
    }

    /// Removes placements assigned to a departed agent while retaining empty
    /// placement entries as VM intent for periodic reconciliation.
    pub(super) fn remove_agent_placements(
        agent_id: ActorId,
        placements: &mut AHashMap<Ulid, Vec<VmPlacement>>,
    ) {
        for entries in placements.values_mut() {
            entries.retain(|entry| entry.agent_id != agent_id);
            Self::shrink_non_migrating_entries(entries);
        }
    }

    /// Rolls back optimistic create state only when no VM actor was created.
    ///
    /// An actor may exist even if the create request failed or its reply was lost;
    /// retaining the state in that case lets normal discovery reconcile it.
    pub(super) fn rollback_failed_create(
        vmid: Ulid,
        actor_exists: bool,
        actor_id: Option<ActorId>,
        actor_map: &mut AHashMap<ActorId, Ulid>,
        manifests: &mut AHashMap<Ulid, VmManifest>,
        placements: &mut AHashMap<Ulid, Vec<VmPlacement>>,
        data_cache: &mut AHashMap<Ulid, Vec<CachedVMActor>>,
    ) {
        if !actor_exists {
            if let Some(actor_id) = actor_id
                && actor_map.get(&actor_id) == Some(&vmid)
            {
                actor_map.remove(&actor_id);
            }
            Self::remove_vm_state(vmid, manifests, placements, data_cache);
        }
    }

    /// Aborts polling and removes all cache state owned by a departed agent.
    pub(super) fn cleanup_agent_actor(&mut self, actor_id: ActorId) {
        if let Some(keepalive_task) = self.agent_keepalive_tasks.remove(&actor_id) {
            trace!(?actor_id, "Aborting agent keepalive task");
            keepalive_task.abort();
        }
        self.agent_data_cache.remove(&actor_id);
        self.agent_vm_index.remove(&actor_id);
        self.invalidate_pending_resources();
        Self::remove_agent_placements(actor_id, &mut self.vm_placements);
    }

    /// Aborts VM polling and makes an unrepresented VM placement recoverable.
    ///
    /// A VM actor is not associated with a particular migration entry. Therefore
    /// placements are cleared only when no discovered actor remains for that VM;
    /// pending destination placements are retained to avoid racing an in-flight
    /// migration. Empty placement entries retain the manifest's desired intent
    /// for periodic reconciliation.
    pub(super) fn cleanup_vm_actor(&mut self, actor_id: ActorId) {
        if let Some(keepalive_task) = self.vm_keepalive_tasks.remove(&actor_id) {
            trace!(?actor_id, "Aborting VM keepalive task");
            keepalive_task.abort();
        }
        let vmid = self.vm_actorid_ulid_map.remove(&actor_id);
        self.invalidate_pending_resources();
        Self::remove_vm_actor(actor_id, &mut self.vm_data_cache);

        let Some(vmid) = vmid else {
            return;
        };
        let has_discovered_actor = self
            .vm_data_cache
            .get(&vmid)
            .is_some_and(|entries| entries.iter().any(|entry| entry.actor_ref.is_some()));
        if !has_discovered_actor {
            if let Some(entries) = self.vm_placements.get_mut(&vmid) {
                entries.retain(|entry| entry.lifecycle == VmLifecycle::Pending);
                Self::shrink_non_migrating_entries(entries);
            }
            self.invalidate_pending_resources();
        }
    }

    /// Incorporates additions and removals from a status delta into placement observations.
    ///
    /// A removal is authoritative for a confirmed (`Running`) placement on this
    /// agent. Pending placements are retained through their timeout because a
    /// create can race the next status delta.
    pub(super) fn reconcile_agent_delta(
        agent_id: ActorId,
        added: &[Ulid],
        removed: &[Ulid],
        manifests: &AHashMap<Ulid, VmManifest>,
        placements: &mut AHashMap<Ulid, Vec<VmPlacement>>,
    ) {
        let now = Instant::now();
        for vmid in added {
            if !manifests.contains_key(vmid) {
                continue;
            }
            let entries = placements.entry(*vmid).or_default();
            if let Some(entry) = entries.iter_mut().find(|entry| entry.agent_id == agent_id) {
                entry.lifecycle = VmLifecycle::Running;
                entry.last_confirmed_at = Some(now);
            } else {
                entries.push(VmPlacement {
                    agent_id,
                    lifecycle: VmLifecycle::Running,
                    created_at: now,
                    last_confirmed_at: Some(now),
                });
            }
        }

        for vmid in removed {
            if let Some(entries) = placements.get_mut(vmid) {
                entries.retain(|entry| {
                    entry.agent_id != agent_id || entry.lifecycle == VmLifecycle::Pending
                });
                Self::shrink_non_migrating_entries(entries);
            }
        }
    }

    /// Reconciles scheduler placement observations with a complete agent snapshot.
    ///
    /// Known, reported VMs gain or refresh `Running` placements. Unknown VMs are
    /// ignored because the scheduler has no retained intent for them. An absent
    /// running placement expires after its confirmation timeout; an absent pending
    /// placement stays reserved through its pending timeout.
    pub(super) fn reconcile_agent_placements(
        agent_id: ActorId,
        status: &AgentStatus,
        manifests: &AHashMap<Ulid, VmManifest>,
        placements: &mut AHashMap<Ulid, Vec<VmPlacement>>,
    ) {
        let now = Instant::now();
        let observed: AHashSet<_> = status.vms.iter().copied().collect();
        let missing_agent_placements: Vec<_> = observed
            .iter()
            .filter(|vmid| manifests.contains_key(vmid))
            .filter(|vmid| {
                placements
                    .get(vmid)
                    .is_none_or(|entries| !entries.iter().any(|entry| entry.agent_id == agent_id))
            })
            .copied()
            .collect();
        for vmid in missing_agent_placements {
            placements.entry(vmid).or_default().push(VmPlacement {
                agent_id,
                lifecycle: VmLifecycle::Running,
                created_at: now,
                last_confirmed_at: Some(now),
            });
        }

        let empty_vmids: Vec<_> = placements
            .iter_mut()
            .filter_map(|(vmid, entries)| {
                entries.retain_mut(|entry| {
                    if entry.agent_id != agent_id || observed.contains(vmid) {
                        if entry.agent_id == agent_id {
                            entry.lifecycle = VmLifecycle::Running;
                            entry.last_confirmed_at = Some(now);
                        }
                        return true;
                    }

                    match entry.lifecycle {
                        VmLifecycle::Pending => {
                            now.duration_since(entry.created_at) < UNRESOLVED_VM_CACHE_TIMEOUT
                        }
                        VmLifecycle::Running => {
                            entry.last_confirmed_at.is_some_and(|last_confirmed_at| {
                                now.duration_since(last_confirmed_at)
                                    < UNCONFIRMED_RUNNING_PLACEMENT_TIMEOUT
                            })
                        }
                    }
                });
                Self::shrink_non_migrating_entries(entries);
                entries.is_empty().then_some(*vmid)
            })
            .collect();
        for vmid in empty_vmids {
            placements.remove(&vmid);
        }
    }

    #[expect(dead_code, reason = "reserved for explicit placement by actor id")]
    fn lookup_agent_by_actor_id(&self, actor_id: &ActorId) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_data_cache
            .get(actor_id)
            .map(|data| data.actor_ref.clone())
    }

    #[expect(dead_code, reason = "reserved for explicit placement by hostname")]
    fn lookup_agent_by_hostname(&self, hostname: &str) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_data_cache
            .values()
            .find(|data| data.data.hostname == hostname)
            .map(|data| data.actor_ref.clone())
    }
}
