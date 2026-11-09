use bytesize::ByteSize;
use kameo::Reply;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use ulid::Ulid;

use crate::types::ObjectMetadata;

/// Reclaim node-local composefs repository space. Images used by any known VM
/// or pinned by persistent upper state stay; everything else is removed.
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct CollectRepositoryGarbage {
    /// Report what would be removed without deleting anything.
    pub dry_run: bool,
}

/// Statistics for one repository GC pass on one node.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryGcReport {
    /// Content-addressed objects removed (or that would be removed).
    pub objects_removed: u64,
    /// Total bytes of removed object data.
    pub objects_bytes: u64,
    /// Broken image symlinks pruned in the repository.
    pub images_pruned: u64,
    /// Broken stream symlinks pruned in the repository.
    pub streams_pruned: u64,
}

/// Result of a GC pass on one agent; `report` is `None` when it failed.
#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct CollectRepositoryGarbageReply {
    pub report: Option<RepositoryGcReport>,
    pub error: Option<String>,
}

/// Result of a cluster-wide GC pass, one entry per known agent.
#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct RepositoryGcReply {
    pub dry_run: bool,
    pub agents: Vec<AgentRepositoryGcOutcome>,
}

/// One node's outcome in a cluster-wide GC pass.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentRepositoryGcOutcome {
    pub hostname: String,
    pub report: Option<RepositoryGcReport>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct GetAgentStatus {
    /// Membership revision already applied by the caller.
    pub since_revision: u64,
    /// Requests the initial full snapshot. Later requests can use revision zero
    /// without forcing a full snapshot when the agent has not changed.
    pub initial: bool,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct AgentStatus {
    pub hostname: String,
    /// Total number of vCPUs before over-provisionment.
    pub vcpus: u32,
    pub ram: ByteSize,
    pub used_vcpus: u32,
    pub used_ram: ByteSize,
    pub vms: Vec<Ulid>,
    pub metadata: ObjectMetadata,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub enum AgentStatusUpdate {
    Full {
        revision: u64,
        status: AgentStatus,
    },
    Delta {
        revision: u64,
        added: Vec<Ulid>,
        removed: Vec<Ulid>,
        used_vcpus: u32,
        used_ram: ByteSize,
    },
}

#[derive(Debug, Clone)]
pub struct MembershipChange {
    pub revision: u64,
    pub vmid: Ulid,
    pub added: bool,
}

pub const STATUS_CHANGE_HISTORY_LIMIT: usize = 256;

pub type StatusChangeHistory = VecDeque<MembershipChange>;

pub fn apply_status_update(status: &mut AgentStatus, update: AgentStatusUpdate) -> u64 {
    match update {
        AgentStatusUpdate::Full {
            revision,
            status: next,
        } => {
            *status = next;
            revision
        }
        AgentStatusUpdate::Delta {
            revision,
            added,
            removed,
            used_vcpus,
            used_ram,
        } => {
            for vmid in removed {
                if let Ok(index) = status.vms.binary_search(&vmid) {
                    status.vms.remove(index);
                }
            }
            for vmid in added {
                match status.vms.binary_search(&vmid) {
                    Ok(_) => {}
                    Err(index) => status.vms.insert(index, vmid),
                }
            }
            status.used_vcpus = used_vcpus;
            status.used_ram = used_ram;
            revision
        }
    }
}
