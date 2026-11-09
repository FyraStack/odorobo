//! Manager-side network object registry and its message handlers.
//!
//! The registry is ephemeral process memory, mirroring how VM intent is held
//! by the scheduler today: agents are told the desired state, and the manager
//! keeps the authoritative view for the lifetime of the process.

use ahash::AHashSet;
use kameo::prelude::*;
use tracing::{info, warn};

use crate::actors::agent_actor::AgentActor;

use crate::messages::network::{
    CreateNetwork, CreateNetworkReply, DeleteNetwork, DeleteNetworkReply, EnsureNetwork,
    GetNetwork, GetNetworkReply, ListNetworks, ListNetworksReply, RemoveNetwork,
};
use crate::network::{Network, NetworkProvider, NetworkState, VNI_RANGE, vni_valid};
use std::{
    hash::{Hash, Hasher},
    ops::RangeInclusive,
};

use super::SchedulerActor;

const VNI_POOL_SIZE: u32 = VNI_RANGE
    .end()
    .saturating_sub(*VNI_RANGE.start())
    .saturating_add(1);

/// VNIs used by the existing PVE SDN fabric.
const PVE_VNI_RANGE: RangeInclusive<u32> = 10_001..=11_002;

fn vni_allocatable(vni: u32) -> bool {
    vni_valid(vni) && !PVE_VNI_RANGE.contains(&vni)
}

/// Hashes the network ID, then probes for a usable free VNI with wraparound.
fn assign_vni(id: &str, taken: impl Fn(u32) -> bool) -> Option<u32> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    let span = u64::from(VNI_POOL_SIZE);
    let offset = u32::try_from(hasher.finish().checked_rem(span).unwrap_or(0)).unwrap_or(0);
    let candidate = VNI_RANGE.start().saturating_add(offset);
    (candidate..=*VNI_RANGE.end())
        .chain(*VNI_RANGE.start()..candidate)
        .find(|&candidate| vni_allocatable(candidate) && !taken(candidate))
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

/// Replays the registry onto an agent the manager has just found, such as a late joiner or a
/// restarted agent; failures are logged, not retried.
pub(super) fn sync_networks_to(
    scheduler: &SchedulerActor,
    hostname: String,
    actor_ref: RemoteActorRef<AgentActor>,
) {
    let networks: Vec<Network> = scheduler.networks.values().cloned().collect();
    if networks.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for network in networks {
            let network_id = network.id.clone();
            let error = match actor_ref.ask(&EnsureNetwork { network }).await {
                Ok(reply) => reply.error,
                Err(err) => Some(err.to_string()),
            };
            if let Some(error) = error {
                warn!(%hostname, %network_id, %error, "agent did not take a network");
            } else {
                info!(%hostname, %network_id, "synced network to agent");
            }
        }
    });
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
            let taken: AHashSet<u32> = self.networks.values().filter_map(Network::vni).collect();
            let Some(vni) = assign_vni(&network.id, |candidate| taken.contains(&candidate)) else {
                return CreateNetworkReply {
                    network: None,
                    error: Some(format!(
                        "the VNI pool {}-{} is exhausted",
                        VNI_RANGE.start(),
                        VNI_RANGE.end()
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

    #[test]
    fn assign_vni_skips_reserved_ranges_when_probing_wraps() {
        let target = PVE_VNI_RANGE.end().saturating_add(1);
        let first = assign_vni("devnet", |_| false).unwrap();
        assert!(first > target);

        let assigned = assign_vni("devnet", |candidate| {
            assert!(vni_valid(candidate));
            assert!(!PVE_VNI_RANGE.contains(&candidate));
            candidate != target
        });
        assert_eq!(assigned, Some(target));
    }
}
