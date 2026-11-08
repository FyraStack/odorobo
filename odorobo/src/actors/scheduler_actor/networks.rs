//! Manager-side network object registry and its message handlers.
//!
//! The registry is ephemeral process memory, mirroring how VM intent is held
//! by the scheduler today: agents are told the desired state, and the manager
//! keeps the authoritative view for the lifetime of the process.

use kameo::prelude::*;
use tracing::info;

use crate::messages::network::{
    CreateNetwork, CreateNetworkReply, DeleteNetwork, DeleteNetworkReply, EnsureNetwork,
    GetNetwork, GetNetworkReply, ListNetworks, ListNetworksReply, RemoveNetwork,
};
use crate::network::Network;

use super::SchedulerActor;

/// Pushes one network to every known agent, returning per-agent failures.
async fn ensure_on_agents(scheduler: &SchedulerActor, network: &Network) -> Vec<String> {
    futures_util::future::join_all(
        scheduler
            .agent_data_cache
            .values()
            .map(|agent| (agent.data.hostname.clone(), agent.actor_ref.clone()))
            .map(|(hostname, actor_ref)| {
                let network = network.clone();
                async move {
                    match actor_ref.ask(&EnsureNetwork { network }).await {
                        Ok(reply) => reply.error.map(|error| format!("{hostname}: {error}")),
                        Err(err) => Some(format!("{hostname}: {err}")),
                    }
                }
            }),
    )
    .await
    .into_iter()
    .flatten()
    .collect()
}

/// Removes one network from every known agent, returning per-agent failures.
async fn remove_from_agents(scheduler: &SchedulerActor, id: &str) -> Vec<String> {
    futures_util::future::join_all(
        scheduler
            .agent_data_cache
            .values()
            .map(|agent| (agent.data.hostname.clone(), agent.actor_ref.clone()))
            .map(|(hostname, actor_ref)| {
                let id = id.to_owned();
                async move {
                    match actor_ref.ask(&RemoveNetwork { id }).await {
                        Ok(reply) => reply.error.map(|error| format!("{hostname}: {error}")),
                        Err(err) => Some(format!("{hostname}: {err}")),
                    }
                }
            }),
    )
    .await
    .into_iter()
    .flatten()
    .collect()
}

/// Creates a network and applies it to every known agent.
///
/// The registry is only updated once every node accepted the object; a partial
/// failure leaves the manager unchanged and reports the failing nodes.
impl Message<CreateNetwork> for SchedulerActor {
    type Reply = CreateNetworkReply;

    async fn handle(
        &mut self,
        msg: CreateNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let network = msg.network;
        if let Err(err) = network.validate() {
            return CreateNetworkReply {
                error: Some(err.to_string()),
            };
        }
        if self.networks.contains_key(&network.id) {
            return CreateNetworkReply {
                error: Some(format!("network {} already exists", network.id)),
            };
        }

        let failures = ensure_on_agents(self, &network).await;
        if !failures.is_empty() {
            return CreateNetworkReply {
                error: Some(failures.join("; ")),
            };
        }

        info!(network = %network.id, provider = ?network.provider, "created network");
        self.networks.insert(network.id.clone(), network);
        CreateNetworkReply { error: None }
    }
}

/// Deletes a network, refusing while an agent still has NICs on it.
impl Message<DeleteNetwork> for SchedulerActor {
    type Reply = DeleteNetworkReply;

    async fn handle(
        &mut self,
        msg: DeleteNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if !self.networks.contains_key(&msg.id) {
            return DeleteNetworkReply {
                error: Some(format!("network {} is not defined", msg.id)),
            };
        }

        let failures = remove_from_agents(self, &msg.id).await;
        if !failures.is_empty() {
            return DeleteNetworkReply {
                error: Some(failures.join("; ")),
            };
        }

        info!(network = %msg.id, "deleted network");
        self.networks.remove(&msg.id);
        DeleteNetworkReply { error: None }
    }
}

impl Message<GetNetwork> for SchedulerActor {
    type Reply = GetNetworkReply;

    async fn handle(
        &mut self,
        msg: GetNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        GetNetworkReply {
            network: self.networks.get(&msg.id).cloned(),
        }
    }
}

impl Message<ListNetworks> for SchedulerActor {
    type Reply = ListNetworksReply;

    async fn handle(
        &mut self,
        _msg: ListNetworks,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut networks: Vec<Network> = self.networks.values().cloned().collect();
        networks.sort_by(|left, right| left.id.cmp(&right.id));
        ListNetworksReply { networks }
    }
}
