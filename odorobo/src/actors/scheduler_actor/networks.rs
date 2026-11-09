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
use crate::network::{Network, NetworkProvider, NetworkState};
use std::hash::{Hash, Hasher};

use super::SchedulerActor;

/// VNI pool the manager assigns from, avoiding PVE SDN's 10001-11002.
const VNI_POOL_START: u32 = 20_000;
const VNI_POOL_END: u32 = 29_999;
const VNI_POOL_SIZE: u32 = 10_000;

/// Picks a VNI for a network id: a hash of the id, then linear probing on
/// collision. The assignment is process-local; nothing durable stores it.
fn assign_vni(id: &str, taken: impl Fn(u32) -> bool) -> Option<u32> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    let span = u64::from(VNI_POOL_SIZE);
    let offset = u32::try_from(hasher.finish().checked_rem(span).unwrap_or(0)).unwrap_or(0);
    let mut candidate = VNI_POOL_START.saturating_add(offset);
    for _ in 0..VNI_POOL_SIZE {
        if !taken(candidate) {
            return Some(candidate);
        }
        candidate = if candidate == VNI_POOL_END {
            VNI_POOL_START
        } else {
            candidate.saturating_add(1)
        };
    }
    None
}

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
/// Runtime state is the manager's, never input; the registry is only updated
/// once every node accepted the object, so a partial failure leaves the
/// manager unchanged and reports the failing nodes.
impl Message<CreateNetwork> for SchedulerActor {
    type Reply = CreateNetworkReply;

    async fn handle(
        &mut self,
        msg: CreateNetwork,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut network = msg.network;
        network.state = None;
        if let Err(err) = network.validate() {
            return CreateNetworkReply {
                network: None,
                error: Some(err.to_string()),
            };
        }
        if self.networks.contains_key(&network.id) {
            return CreateNetworkReply {
                network: None,
                error: Some(format!("network {} already exists", network.id)),
            };
        }

        // The VNI is derived runtime state: the manager assigns it, agents
        // realize it, and nothing durable stores it.
        if network.provider == NetworkProvider::Routed {
            let Some(vni) = assign_vni(&network.id, |candidate| {
                self.networks
                    .values()
                    .any(|existing| existing.vni() == Some(candidate))
            }) else {
                return CreateNetworkReply {
                    network: None,
                    error: Some(format!(
                        "the VNI pool {VNI_POOL_START}-{VNI_POOL_END} is exhausted"
                    )),
                };
            };
            network.state = Some(NetworkState { vni });
        }

        let failures = ensure_on_agents(self, &network).await;
        if !failures.is_empty() {
            return CreateNetworkReply {
                network: None,
                error: Some(failures.join("; ")),
            };
        }

        info!(
            network = %network.id,
            provider = ?network.provider,
            vni = ?network.vni(),
            "created network"
        );
        let assigned = network.clone();
        self.networks.insert(network.id.clone(), network);
        CreateNetworkReply {
            network: Some(assigned),
            error: None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assign_vni_probes_past_taken_candidates() {
        let first = assign_vni("devnet", |_| false).unwrap();
        let second = assign_vni("devnet", |candidate| candidate == first).unwrap();
        assert_ne!(first, second);
    }
}
