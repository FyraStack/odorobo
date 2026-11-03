//! delete bookkeeping that doesn't need the network: tombstones, deciding what
//! to do with each cleanup owner's reply, background deletes, and pruning

use std::time::{Duration, Instant};

use kameo::prelude::*;
use tracing::warn;
use ulid::Ulid;

use crate::messages::agent::AgentStatusUpdate;

use super::{AutoDeleteState, SchedulerActor, VmDeleteOwner, VmTombstone, VmTombstoneState};

/// stopped VMs only need their tombstone long enough to ignore late discovery
pub(super) const STOPPED_TOMBSTONE_TTL: Duration = Duration::from_hours(1);
/// deleted VMs keep theirs longer so a node that comes back late still gets
/// its leftovers cleaned up in the background
pub(super) const DELETED_TOMBSTONE_TTL: Duration = Duration::from_hours(24);
/// how long to wait before retrying a background delete that failed
pub(super) const AUTO_DELETE_RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// a background delete that takes longer than this counts as failed
pub(super) const AUTO_DELETE_TIMEOUT: Duration = Duration::from_mins(5);

/// one route's answer when we asked an owner to delete a VM
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeleteAttempt {
    Acked,
    /// the agent is alive and explicitly said cleanup failed
    AgentError(String),
    /// transport or ask failure, so we don't know if the agent is even there
    Unreachable(String),
}

/// what all of an owner's routes added up to
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeleteOwnerOutcome {
    Acked,
    /// at least one live agent rejected the delete. even force can't skip this
    Rejected(Vec<String>),
    /// nothing answered. empty means we didn't even have a route
    Unreachable(Vec<String>),
}

/// what to do with every owner after one delete pass
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DeleteSettlement {
    pub acked: Vec<VmDeleteOwner>,
    /// owners force let us give up on, with why
    pub dropped: Vec<(VmDeleteOwner, String)>,
    pub failures: Vec<String>,
}

/// folds an owner's per-route attempts into one outcome. any ack wins, then
/// any explicit agent error, since a stale route failing doesn't matter if a
/// live replacement on the same peer answered
pub(super) fn delete_owner_outcome(attempts: Vec<DeleteAttempt>) -> DeleteOwnerOutcome {
    let mut rejected = Vec::new();
    let mut unreachable = Vec::new();
    for attempt in attempts {
        match attempt {
            DeleteAttempt::Acked => return DeleteOwnerOutcome::Acked,
            DeleteAttempt::AgentError(error) => rejected.push(error),
            DeleteAttempt::Unreachable(error) => unreachable.push(error),
        }
    }
    if rejected.is_empty() {
        DeleteOwnerOutcome::Unreachable(unreachable)
    } else {
        DeleteOwnerOutcome::Rejected(rejected)
    }
}

/// decides which owners count as done. without force, anything short of an
/// ack fails the delete. with force, owners we couldn't reach get dropped, but
/// an agent that answered with an error still fails it
pub(super) fn plan_delete_settlement(
    outcomes: Vec<(VmDeleteOwner, DeleteOwnerOutcome)>,
    force: bool,
) -> DeleteSettlement {
    let mut settlement = DeleteSettlement::default();
    for (owner, outcome) in outcomes {
        match outcome {
            DeleteOwnerOutcome::Acked => settlement.acked.push(owner),
            DeleteOwnerOutcome::Rejected(errors) => settlement
                .failures
                .push(format!("{owner:?}: {}", errors.join("; "))),
            DeleteOwnerOutcome::Unreachable(errors) => {
                let reason = if errors.is_empty() {
                    "missing agent for known VM owner".to_owned()
                } else {
                    errors.join("; ")
                };
                if force {
                    settlement.dropped.push((owner, reason));
                } else {
                    settlement.failures.push(format!("{owner:?}: {reason}"));
                }
            }
        }
    }
    settlement
}

/// whether a same-revision delta just refreshes usage and `retained_vms`.
/// those can change without a membership revision bump
pub(super) fn is_refresh_only_update(
    update: &AgentStatusUpdate,
    cached_revision: Option<u64>,
) -> bool {
    matches!(
        update,
        AgentStatusUpdate::Delta { revision, added, removed, .. }
            if cached_revision == Some(*revision) && added.is_empty() && removed.is_empty()
    )
}

/// VM IDs an accepted status update says this agent holds something for, or
/// `None` if the update is stale and gets ignored. only snapshots, additions,
/// and `retained_vms` count. a removal doesn't tell us anything about a
/// stopped node-local CID lease
pub(super) fn status_update_held_vmids(
    update: &AgentStatusUpdate,
    cached_revision: Option<u64>,
) -> Option<Vec<Ulid>> {
    match update {
        AgentStatusUpdate::Full { revision, status }
            if cached_revision.is_none_or(|cached| *revision > cached) =>
        {
            Some(
                status
                    .vms
                    .iter()
                    .chain(&status.retained_vms)
                    .copied()
                    .collect(),
            )
        }
        AgentStatusUpdate::Delta {
            revision,
            added,
            retained_vms,
            ..
        } if is_refresh_only_update(update, cached_revision)
            || cached_revision.is_some_and(|cached| *revision > cached) =>
        {
            Some(added.iter().chain(retained_vms).copied().collect())
        }
        AgentStatusUpdate::Full { .. } | AgentStatusUpdate::Delta { .. } => None,
    }
}

impl SchedulerActor {
    /// records the agent as a cleanup owner for every VM its status says it
    /// holds. this is how owner history comes back after a scheduler restart
    pub(super) fn record_status_delete_owners(
        &mut self,
        agent_id: ActorId,
        vmids: impl IntoIterator<Item = Ulid>,
    ) {
        for vmid in vmids {
            if self.should_record_delete_owner(vmid) {
                self.remember_vm_delete_owner(vmid, VmDeleteOwner::agent(agent_id));
            }
        }
    }

    pub(super) fn vm_tombstone_state(&self, vmid: Ulid) -> Option<VmTombstoneState> {
        self.vm_tombstones
            .get(&vmid)
            .map(|tombstone| tombstone.state)
    }

    pub(super) fn set_vm_tombstone(&mut self, vmid: Ulid, state: VmTombstoneState) {
        self.vm_tombstones.insert(vmid, VmTombstone::new(state));
    }

    /// stopped or fully deleted, so late discovery shouldn't bring the VM back
    /// or re-add cleanup owners
    pub(super) fn vm_is_stopped_or_deleted(&self, vmid: Ulid) -> bool {
        matches!(
            self.vm_tombstone_state(vmid),
            Some(VmTombstoneState::Stopped | VmTombstoneState::Deleted)
        )
    }

    /// whether agent status or late VM discovery should count as a cleanup
    /// owner. a live or delete-pending VM always does, a stopped/deleted one
    /// only if we're still tracking its owners
    pub(super) fn should_record_delete_owner(&self, vmid: Ulid) -> bool {
        !self.vm_is_stopped_or_deleted(vmid) || self.vm_delete_targets.contains_key(&vmid)
    }

    /// forgets an owner force let us give up on. if it comes back and still
    /// reports the VM, the status handler deletes it in the background
    pub(super) fn drop_vm_delete_owner(&mut self, vmid: Ulid, owner: VmDeleteOwner) {
        if let Some(targets) = self.vm_delete_targets.get_mut(&vmid) {
            targets.remove(&owner);
        }
    }

    /// like `finish_vm_delete`, but also finishes when force dropped every
    /// owner or there never were any
    pub(super) fn force_finish_vm_delete(&mut self, vmid: Ulid) -> bool {
        if self
            .vm_delete_targets
            .get(&vmid)
            .is_some_and(|targets| targets.values().any(|target| !target.confirmed))
        {
            return false;
        }
        self.vm_delete_targets.remove(&vmid);
        self.set_vm_tombstone(vmid, VmTombstoneState::Deleted);
        true
    }

    /// records acks and drops from one delete pass, then finishes the delete if
    /// nothing failed. acks stick even when another owner failed
    pub(super) fn apply_delete_settlement(
        &mut self,
        vmid: Ulid,
        settlement: DeleteSettlement,
        force: bool,
    ) -> Result<(), String> {
        for owner in settlement.acked {
            self.acknowledge_vm_delete_owner(vmid, owner);
        }
        for (owner, reason) in settlement.dropped {
            warn!(
                %vmid,
                ?owner,
                %reason,
                "force delete is dropping an unreachable cleanup owner, its node-local state might leak"
            );
            self.drop_vm_delete_owner(vmid, owner);
        }
        if !settlement.failures.is_empty() {
            return Err(format!(
                "failed to release VM CID on one or more agents: {}",
                settlement.failures.join("; ")
            ));
        }
        let finished = if force {
            self.force_finish_vm_delete(vmid)
        } else {
            self.finish_vm_delete(vmid)
        };
        if finished {
            Ok(())
        } else {
            Err("no agents can confirm VM teardown and CID release".to_owned())
        }
    }

    /// picks VMs an agent still reports that we already finished deleting, and
    /// marks each one in flight. at most one per (VM, agent) at a time, and a
    /// failed one waits `AUTO_DELETE_RETRY_INTERVAL` before going again
    pub(super) fn take_auto_delete_vmids(
        &mut self,
        agent_id: ActorId,
        reported: impl IntoIterator<Item = Ulid>,
        now: Instant,
    ) -> Vec<Ulid> {
        let mut vmids = Vec::new();
        for vmid in reported {
            if self.vm_tombstone_state(vmid) != Some(VmTombstoneState::Deleted) {
                continue;
            }
            let ready = match self.vm_auto_deletes.get(&(vmid, agent_id)) {
                None => true,
                Some(AutoDeleteState::InFlight) => false,
                Some(AutoDeleteState::Failed(at)) => {
                    now.saturating_duration_since(*at) >= AUTO_DELETE_RETRY_INTERVAL
                }
            };
            if ready {
                self.vm_auto_deletes
                    .insert((vmid, agent_id), AutoDeleteState::InFlight);
                vmids.push(vmid);
            }
        }
        vmids
    }

    /// clears the in-flight marker. failures stay around so the next status
    /// report after the retry interval tries again
    pub(super) fn settle_auto_delete(
        &mut self,
        vmid: Ulid,
        agent_id: ActorId,
        error: Option<String>,
        now: Instant,
    ) {
        if let Some(error) = error {
            warn!(%vmid, ?agent_id, %error, "background delete of a deleted VM failed, will retry");
            self.vm_auto_deletes
                .insert((vmid, agent_id), AutoDeleteState::Failed(now));
        } else {
            self.vm_auto_deletes.remove(&(vmid, agent_id));
        }
    }

    /// whether a background delete for this VM could still be running on some agent
    pub(super) fn auto_delete_in_flight(&self, vmid: Ulid) -> bool {
        self.vm_auto_deletes
            .iter()
            .any(|((id, _), state)| *id == vmid && *state == AutoDeleteState::InFlight)
    }

    /// drops old `Stopped`/`Deleted` tombstones and expired retry markers.
    /// `DeletePending` and anything we still track owners for are kept no
    /// matter how old
    pub(super) fn prune_vm_tombstones(&mut self, now: Instant) {
        let delete_targets = &self.vm_delete_targets;
        self.vm_tombstones.retain(|vmid, tombstone| {
            let ttl = match tombstone.state {
                VmTombstoneState::DeletePending => return true,
                VmTombstoneState::Stopped => STOPPED_TOMBSTONE_TTL,
                VmTombstoneState::Deleted => DELETED_TOMBSTONE_TTL,
            };
            delete_targets.contains_key(vmid)
                || now.saturating_duration_since(tombstone.since) < ttl
        });
        self.vm_auto_deletes.retain(|_, state| match state {
            AutoDeleteState::InFlight => true,
            AutoDeleteState::Failed(at) => {
                now.saturating_duration_since(*at) < AUTO_DELETE_RETRY_INTERVAL
            }
        });
    }
}
