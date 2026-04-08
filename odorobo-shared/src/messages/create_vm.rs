use cloud_hypervisor_client::models::VmConfig;
use kameo::Reply;
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
    pub vm_id: Ulid,
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
