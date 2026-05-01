//! VM-related messages
use cloud_hypervisor_client::models::VmConfig;
use kameo::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::types::VirtualMachine;

// TODO: when scheduler does createVM it also stores which server we put the Ulid on so it can do a in memory cache and doesn't need to hit the Server
//  for failover, the new node when it fails over will need to rebuild this cache via hitting a GetAllVMs message on every server
//  additionally, when the VmConfig is created, this determines the MAC address of the server. meaning as soon as we have this info, we need to hit the router via the scheduler, because the router might be slow.
/// Message to create a new VM
///
/// VmConfig is a Cloud Hypervisor VM spec, containing the VM's full configuration (untransformed by odorobo)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreateVM {
    /// the ULID of the VM to create
    pub vmid: Ulid,
    /// VmConfig in message, untransformed.
    ///
    /// Transformer API will transform this VmConfig into proper
    /// node-specific, paths, i.e attach LUNs, networking?
    ///
    /// this data would go to state::instance::spawn()
    pub config: VirtualMachine,
}

#[derive(Serialize, Deserialize, Reply, Debug, JsonSchema)]
pub struct CreateVMReply {
    pub config: Option<VirtualMachine>,
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
    pub config: VmConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrepMigration {
    pub vmid: Ulid,
    pub config: VmConfig,
}

/// Reply to a MigrateVMReceive message, containing the listening address of the VM
#[derive(Serialize, Deserialize, Debug, Clone, Reply)]
pub struct MigrateVMReceiveReply {
    pub listening_address: String,
}

/// Message to delete a VM
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeleteVM {
    pub vmid: Ulid,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct DeleteVMReply;

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
#[derive(Serialize, Deserialize, Debug)]
pub struct GetVMInfo {
    pub vmid: Option<Ulid>,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct GetVMInfoReply {
    pub vmid: Ulid,
    pub config: Option<VmConfig>,
}
