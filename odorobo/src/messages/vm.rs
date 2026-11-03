//! VM-related messages

use kameo::prelude::*;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::manifest::VmManifest;

/// Message to create a new VM
///
/// The message carries provider-neutral VM intent. The destination agent
/// translates it into Cloud Hypervisor configuration locally.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreateVM {
    /// the ULID of the VM to create
    pub vmid: Ulid,
    /// Provider-neutral VM intent. Cloud Hypervisor conversion happens in the
    /// Cloud Hypervisor driver on the destination agent.
    pub config: VmManifest,
    /// scheduler only. a time-ordered stamp for this incarnation of the VM, so
    /// the agent can tell a delete meant for an older one apart from one meant
    /// for this one. see [`DeleteVM::issued_at`]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<Ulid>,
}

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct CreateVMReply {
    pub config: Option<VmManifest>,
    /// Serialized ID of the VM actor created by the agent.
    pub actor_id: Option<Vec<u8>>,
    /// VM startup error from the destination agent, if there was one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Message to delete a VM's config from the agent, shutting it down
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CloudHypervisorDeleteVMConfig {
    pub vmid: Ulid,
}

/// Message to migrate a VM to a destination
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MigrateVMSend {
    pub vmid: Ulid,
    pub target: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MigrateVMReceive {
    pub vmid: Ulid,
    pub config: VmManifest,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrepMigration {
    pub vmid: Ulid,
    pub config: VmManifest,
}

/// Reply to a migration receive request. A non-empty `error` means no valid
/// receive operation was started and `listening_address` is empty.
#[derive(Serialize, Deserialize, Debug, Clone, Reply)]
pub struct MigrateVMReceiveReply {
    /// Address the source should connect to when migration receive started.
    pub listening_address: String,
    /// Structured operation failure returned without panicking the VM actor.
    pub error: Option<String>,
}

/// Message to delete a VM
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeleteVM {
    pub vmid: Ulid,
    /// scheduler only, agents ignore this. drops cleanup owners whose agent
    /// can't be reached at all (not ones that just timed out) instead of
    /// failing the delete, so a node that's gone for
    /// good doesn't make its VMs undeletable. anything that node still holds is
    /// leaked until it comes back and gets cleaned up in the background
    #[serde(default)]
    pub force: bool,
    /// scheduler only. stamped from the same clock as [`CreateVM::generation`]
    /// when the scheduler sends the delete. a delete that sat in transit (or in
    /// the agent's mailbox) while the ID got recreated is older than the new
    /// VM's generation, and the agent acks it without touching the new VM
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_at: Option<Ulid>,
}

impl DeleteVM {
    /// whether this delete was sent before a VM with this generation was
    /// created, so it can only have been meant for an older incarnation. if
    /// either side isn't stamped we can't tell, and the delete goes ahead
    #[must_use]
    pub fn predates(&self, generation: Option<Ulid>) -> bool {
        matches!((self.issued_at, generation), (Some(issued_at), Some(generation)) if issued_at < generation)
    }
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct DeleteVMReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Shuts down a VM temporarily
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShutdownVM {
    pub vmid: Ulid,
}

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct ShutdownVMReply;

/// List VMs on an agent
#[derive(Serialize, Deserialize, Debug)]
pub struct AgentListVMs;

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct AgentListVMsReply {
    // list VMs
    pub vms: Vec<Ulid>,
}

/// Get VM info
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetVMInfo {
    pub vmid: Option<Ulid>,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct GetVMInfoReply {
    pub vmid: Ulid,
    pub config: Option<VmManifest>,
}

/// Lightweight VM liveness check used by the scheduler heartbeat.
#[derive(Serialize, Deserialize, Debug)]
pub struct GetVMHeartbeat;

#[derive(Serialize, Deserialize, Reply, Debug, Clone, Copy)]
pub struct GetVMHeartbeatReply {
    pub vmid: Ulid,
}

/// Retrieve the retained serial-console output for a VM.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetConsoleHistory {
    pub vmid: Ulid,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct GetConsoleHistoryReply {
    pub history: Vec<u8>,
}

/// Send raw input bytes to a VM's serial console.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SendConsoleInput {
    pub vmid: Ulid,
    pub input: Vec<u8>,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct SendConsoleInputReply {
    pub written: usize,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::DeleteVM;
    use ulid::Ulid;

    #[test]
    fn only_a_stamped_delete_older_than_a_stamped_vm_predates_it() {
        let older = Ulid::from_parts(1, 0);
        let newer = Ulid::from_parts(2, 0);
        let delete = |issued_at| DeleteVM {
            vmid: Ulid::nil(),
            force: false,
            issued_at,
        };
        assert!(delete(Some(older)).predates(Some(newer)));
        assert!(!delete(Some(newer)).predates(Some(older)));
        assert!(!delete(Some(older)).predates(Some(older)));
        // anything unstamped (old schedulers, migrated VMs) gets deleted as before
        assert!(!delete(None).predates(Some(newer)));
        assert!(!delete(Some(older)).predates(None));
    }
}
