use bytesize::ByteSize;
use kameo::Reply;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use ulid::Ulid;

use crate::types::ObjectMetadata;

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
