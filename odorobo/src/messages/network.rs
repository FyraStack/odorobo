//! Network object messages

use kameo::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::network::Network;

/// Adds or updates a network object on an agent.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnsureNetwork {
    /// Desired state for the network.
    pub network: Network,
}

/// Reply to [`EnsureNetwork`]. A non-empty `error` means nothing was changed.
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

/// Reply to [`RemoveNetwork`]. A non-empty `error` means nothing was changed.
#[derive(Serialize, Deserialize, Reply, Debug, Clone, Default)]
pub struct RemoveNetworkReply {
    pub error: Option<String>,
}

/// Creates or updates a network in the manager.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreateNetwork {
    /// Desired state for the network.
    pub network: Network,
}

/// Reply to [`CreateNetwork`]. A non-empty `error` means nothing was changed.
#[derive(Serialize, Deserialize, Reply, Debug, Clone, Default)]
pub struct CreateNetworkReply {
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
