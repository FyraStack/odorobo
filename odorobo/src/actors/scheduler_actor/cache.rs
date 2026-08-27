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

impl SchedulerActor {
    pub(super) fn shrink_non_migrating_entries<T>(entries: &mut Vec<T>) {
        if entries.len() <= 1 && entries.capacity() >= 4 {
            entries.shrink_to_fit();
        }
    }

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

    pub(super) fn cleanup_unresolved_vm_cache(
        manifests: &mut AHashMap<Ulid, VmManifest>,
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
            Self::remove_vm_state(vmid, manifests, placements, data_cache);
        }
    }

    /// Returns every VM that could be on an agent, deduplicating each source in
    /// constant expected time.
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

    pub(super) fn remove_agent_placements(
        agent_id: ActorId,
        manifests: &mut AHashMap<Ulid, VmManifest>,
        placements: &mut AHashMap<Ulid, Vec<VmPlacement>>,
        data_cache: &mut AHashMap<Ulid, Vec<CachedVMActor>>,
    ) {
        let empty_vmids: Vec<_> = placements
            .iter_mut()
            .filter_map(|(vmid, entries)| {
                entries.retain(|entry| entry.agent_id != agent_id);
                Self::shrink_non_migrating_entries(entries);
                entries.is_empty().then_some(*vmid)
            })
            .collect();

        for vmid in empty_vmids {
            Self::remove_vm_state(vmid, manifests, placements, data_cache);
        }
    }

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

    pub(super) fn cleanup_agent_actor(&mut self, actor_id: ActorId) {
        if let Some(keepalive_task) = self.agent_keepalive_tasks.remove(&actor_id) {
            trace!(?actor_id, "Aborting agent keepalive task");
            keepalive_task.abort();
        }
        self.agent_data_cache.remove(&actor_id);
        self.agent_vm_index.remove(&actor_id);
        self.invalidate_pending_resources();
        Self::remove_agent_placements(
            actor_id,
            &mut self.vm_manifests,
            &mut self.vm_placements,
            &mut self.vm_data_cache,
        );
    }

    pub(super) fn cleanup_vm_actor(&mut self, actor_id: ActorId) {
        if let Some(keepalive_task) = self.vm_keepalive_tasks.remove(&actor_id) {
            trace!(?actor_id, "Aborting VM keepalive task");
            keepalive_task.abort();
        }
        let vmid = self.vm_actorid_ulid_map.remove(&actor_id);
        self.invalidate_pending_resources();
        Self::remove_vm_actor(actor_id, &mut self.vm_data_cache);
        if let Some(vmid) = vmid
            && self
                .vm_data_cache
                .get(&vmid)
                .is_none_or(|entries| entries.iter().all(|entry| entry.actor_ref.is_none()))
        {
            Self::remove_vm_state(
                vmid,
                &mut self.vm_manifests,
                &mut self.vm_placements,
                &mut self.vm_data_cache,
            );
        }
    }

    pub(super) fn reconcile_agent_delta(
        agent_id: ActorId,
        added: &[Ulid],
        _removed: &[Ulid],
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

        // A removal is an observation about the agent, not a change to the
        // scheduler's desired state. Keep the placement so reconciliation can
        // schedule the VM again. The full status path performs the same
        // distinction for snapshots.
    }

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
                for entry in entries
                    .iter_mut()
                    .filter(|entry| entry.agent_id == agent_id)
                {
                    if observed.contains(vmid) {
                        entry.lifecycle = VmLifecycle::Running;
                        entry.last_confirmed_at = Some(now);
                    }
                }
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
