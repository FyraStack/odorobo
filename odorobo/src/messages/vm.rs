//! VM-related messages

use kameo::prelude::*;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::cluster_state::VMStopFence;
use crate::manifest::VmManifest;

/// Message to create a new VM
///
/// The message carries provider-neutral VM intent. The destination agent
/// translates it into Cloud Hypervisor configuration locally.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreateVM {
    /// the ULID of the VM to create
    pub vmid: Ulid,
    /// Durable incarnation token assigned by the scheduler. User requests omit
    /// it; agent-side reconciliation requires an exact placement match.
    #[serde(default)]
    pub generation: Ulid,
    /// Provider-neutral VM intent. Cloud Hypervisor conversion happens in the
    /// Cloud Hypervisor driver on the destination agent.
    pub config: VmManifest,
}

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct CreateVMReply {
    pub config: Option<VmManifest>,
    /// Serialized ID of the VM actor created by the agent.
    pub actor_id: Option<Vec<u8>>,
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
    /// Present on owner-agent/runtime dispatches; omitted for user requests to
    /// the scheduler, which binds the request to the durable stop intent.
    #[serde(default)]
    pub expected: Option<VMStopFence>,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct DeleteVMReply {
    pub error: Option<String>,
    /// The exact placement incarnation whose teardown completed.
    #[serde(default)]
    pub completed: Option<VMStopFence>,
}

/// Shuts down a VM temporarily
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShutdownVM {
    pub vmid: Ulid,
    /// Present on owner-agent/runtime dispatches; omitted for user requests to
    /// the scheduler, which binds the request to the durable stop intent.
    #[serde(default)]
    pub expected: Option<VMStopFence>,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct ShutdownVMReply {
    /// The exact placement incarnation whose teardown completed.
    pub completed: VMStopFence,
}

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

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct GetVMHeartbeatReply {
    pub vmid: Ulid,
    #[serde(default)]
    pub generation: Ulid,
    pub error: Option<String>,
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
