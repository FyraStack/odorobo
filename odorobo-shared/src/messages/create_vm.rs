use cloud_hypervisor_client::models::VmConfig;
use kameo::prelude::*;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

// TODO: when scheduler does createVM it also stores which server we put the Ulid on so it can do a in memory cache and doesn't need to hit the Server
//  for failover, the new node when it fails over will need to rebuild this cache via hitting a GetAllVMs message on every server
//  additionally, when the VmConfig is created, this determines the MAC address of the server. meaning as soon as we have this info, we need to hit the router via the scheduler, because the router might be slow.
/// Message to create a new VM
///
/// VmConfig is a Cloud Hypervisor VM spec, containing the VM's full configuration (untransformed by odorobo)

#[derive(Serialize, Deserialize, Debug)]
pub struct CreateVM {
    /// the ULID of the VM to create
    pub vmid: Ulid,
    /// VmConfig in message, untransformed.
    ///
    /// Transformer API will transform this VmConfig into proper
    /// node-specific, paths, i.e attach LUNs, networking?
    ///
    /// this data would go to state::instance::spawn()
    pub config: VmConfig,
}

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct CreateVMReply {
    pub config: Option<VmConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeleteVM {
    pub vmid: Ulid,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct DeleteVMReply;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShutdownVM {
    pub vmid: Ulid,
}

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct ShutdownVMReply;

#[derive(Serialize, Deserialize, Debug)]
pub struct AgentListVMs;

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct AgentListVMsReply {
    // list VMs
    pub vms: Vec<Ulid>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GetVMInfo {
    pub vmid: Ulid,
}

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct GetVMInfoReply {
    pub vmid: Ulid,
    pub config: Option<VmConfig>,
}
