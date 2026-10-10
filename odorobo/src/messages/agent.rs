use bytesize::ByteSize;
use kameo::Reply;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use ulid::Ulid;

use crate::types::ObjectMetadata;

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct GetAgentStatus {
    /// Agent status revision (membership and resource accounting) already applied by the caller.
    pub since_revision: u64,
    /// Requests the initial full snapshot. Later requests can use revision zero
    /// without forcing a full snapshot when the agent has not changed.
    pub initial: bool,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone, PartialEq, Eq)]
pub struct VMResourceCharge {
    pub vmid: Ulid,
    pub generation: Ulid,
    pub vcpus: u32,
    pub memory_bytes: u64,
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
    /// VM allocations charged to capacity but not currently in the running VM cache.
    #[serde(default)]
    pub reserved_vms: Vec<Ulid>,
    /// Generation- and resource-specific confirmations for VM allocations included
    /// in `used_vcpus` and `used_ram`. Empty on legacy agents/status records.
    #[serde(default)]
    pub resource_charges: Vec<VMResourceCharge>,
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
        /// Snapshot of actorless/uncertain VM allocations included in used resources.
        #[serde(default)]
        reserved_vms: Vec<Ulid>,
        /// Full charge confirmations snapshot for allocations included in usage.
        #[serde(default)]
        resource_charges: Vec<VMResourceCharge>,
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
            reserved_vms,
            resource_charges,
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
            status.reserved_vms = reserved_vms;
            status.resource_charges = resource_charges;
            status.used_vcpus = used_vcpus;
            status.used_ram = used_ram;
            revision
        }
    }
}
