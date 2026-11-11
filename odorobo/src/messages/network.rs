//! Network object messages

use kameo::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::network::Network;
use ipnet::Ipv4Net;
use std::net::Ipv4Addr;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default)]
pub struct GetVtep;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VtepInfo {
    pub vtep: Ipv4Addr,
    pub underlay: Ipv4Net,
    pub asn: u32,
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct GetVtepReply {
    #[serde(default)]
    pub node_id: Option<String>,
    pub fabric: Option<VtepInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum NetworkUpdate {
    Ensure(Network),
    Remove(Network),
}

impl NetworkUpdate {
    #[must_use]
    pub const fn network(&self) -> &Network {
        match self {
            Self::Ensure(network) | Self::Remove(network) => network,
        }
    }
}

/// A revisioned manager update; removals retain the metadata needed to clean up failed starts.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SyncNetwork {
    pub manager_id: ActorId,
    pub revision: u64,
    pub update: NetworkUpdate,
    pub owners_revision: u64,
    pub active_owner: Option<Network>,
}

impl SyncNetwork {
    #[must_use]
    pub fn same_update(&self, other: &Self) -> bool {
        self.manager_id == other.manager_id
            && self.revision == other.revision
            && self.update == other.update
    }
}

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct SyncNetworkReply {
    pub revision: u64,
    pub error: Option<String>,
}

/// Adds or updates a network object on an agent.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnsureNetwork {
    /// Desired state for the network.
    pub network: Network,
}

/// Reply to [`EnsureNetwork`]; an error means realization did not complete.
#[derive(Serialize, Deserialize, Reply, Debug, Clone, Default)]
pub struct EnsureNetworkReply {
    pub error: Option<String>,
}

/// Removes a network object from an agent.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoveNetwork {
    /// Network identifier.
    pub id: String,
}

/// Reply to [`RemoveNetwork`]; an error means cleanup did not complete.
#[derive(Serialize, Deserialize, Reply, Debug, Clone, Default)]
pub struct RemoveNetworkReply {
    pub error: Option<String>,
}

/// Creates or updates a network in the manager.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreateNetwork {
    /// Desired state for the network
    pub network: Network,
}

/// Reply to [`CreateNetwork`]. A non-empty `error` means nothing was created.
#[derive(Serialize, Deserialize, Reply, Debug, Clone, Default)]
pub struct CreateNetworkReply {
    /// The network with its assigned runtime state.
    pub network: Option<Network>,
    pub error: Option<String>,
}

/// Deletes a network from the manager.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeleteNetwork {
    /// Network identifier.
    pub id: String,
}

/// Reply to [`DeleteNetwork`]. A non-empty `error` means nothing was changed.
#[derive(Serialize, Deserialize, Reply, Debug, Clone, Default)]
pub struct DeleteNetworkReply {
    pub error: Option<String>,
}

/// Looks up a network in the manager.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetNetwork {
    /// Network identifier.
    pub id: String,
}

/// Reply to [`GetNetwork`].
#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct GetNetworkReply {
    pub network: Option<Network>,
}

/// Lists every network known to the manager.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default)]
pub struct ListNetworks;

/// Reply to [`ListNetworks`].
#[derive(Serialize, Deserialize, Reply, Debug, Clone, Default, JsonSchema)]
pub struct ListNetworksReply {
    pub networks: Vec<Network>,
}
