//! VM-related messages

use kameo::prelude::*;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::manifest::{ObservedState, VmManifest};

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
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct DeleteVMReply {
    /// Persistent rootfs teardown or deletion error, if actor cleanup failed.
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
    /// Lifecycle as seen by the VM actor; kept out of `config`, which mirrors desired intent.
    #[serde(default)]
    pub observed: Option<ObservedState>,
}

/// Lightweight VM liveness check used by the scheduler heartbeat.
#[derive(Serialize, Deserialize, Debug)]
pub struct GetVMHeartbeat;

#[derive(Serialize, Deserialize, Reply, Debug, Clone, Copy)]
pub struct GetVMHeartbeatReply {
    pub vmid: Ulid,
}

/// Retrieve the retained serial-console output for a VM: kernel and init logs.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetConsoleHistory {
    pub vmid: Ulid,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct GetConsoleHistoryReply {
    pub history: Vec<u8>,
}

/// Retrieve the retained workload output for a VM: the stdin/stdout/stderr
/// stream of the OCI workload on the virtio console.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetWorkloadLogs {
    pub vmid: Ulid,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct GetWorkloadLogsReply {
    pub logs: Vec<u8>,
}

/// Send raw input bytes to a VM's console.
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
