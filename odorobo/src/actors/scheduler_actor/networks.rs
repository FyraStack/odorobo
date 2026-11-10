//! Manager-side network object registry and its message handlers.
//!
//! The registry is ephemeral process memory, mirroring how VM intent is held
//! by the scheduler today: agents are told the desired state, and the manager
//! keeps the authoritative view for the lifetime of the process.

use ahash::AHashSet;
use kameo::prelude::*;
use tracing::info;

use crate::messages::network::{
    CreateNetwork, CreateNetworkReply, DeleteNetwork, DeleteNetworkReply, GetNetwork,
    GetNetworkReply, ListNetworks, ListNetworksReply, NetworkUpdate,
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
    // Fold the hash into the pool size; the remainder is always a valid offset.
    let slot = hasher
        .finish()
        .checked_rem(u64::from(VNI_POOL_SIZE))
        .unwrap_or_default();
    let offset = u32::try_from(slot).unwrap_or_default();
    let candidate = VNI_RANGE.start().saturating_add(offset);

    // Walk from the hash slot to the pool end, then wrap around to the start.
    (candidate..=*VNI_RANGE.end())
        .chain(*VNI_RANGE.start()..candidate)
        .find(|&vni| vni_allocatable(vni) && !taken(vni))
}

/// Fails with the nodes that still lack a network actor, if any.
fn require_network_actors(scheduler: &SchedulerActor) -> Result<(), String> {
    let missing: Vec<String> = scheduler
        .agent_data_cache
        .values()
        .filter(|agent| !scheduler.network_sync.contains_agent(agent.actor_ref.id()))
        .map(|agent| format!("{}: network actor is not available", agent.data.hostname))
        .collect();
    let error = missing.join("; ");
    if error.is_empty() { Ok(()) } else { Err(error) }
}

/// Creates a network and applies it to every known agent.
impl Message<CreateNetwork> for SchedulerActor {
    type Reply = CreateNetworkReply;

    #[tracing::instrument(level = "debug", skip(self, ctx), fields(network = %msg.network.id))]
    async fn handle(
        &mut self,
        msg: CreateNetwork,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match self.create_network(msg.network, ctx.actor_ref().id()).await {
            Ok(network) => CreateNetworkReply {
                network: Some(network),
                error: None,
            },
            Err(error) => CreateNetworkReply {
                network: None,
                error: Some(error),
            },
        }
    }
}

impl SchedulerActor {
    /// Registers a network and applies it to every known agent.
    ///
    /// Runtime state is the manager's, never input; the registry is only updated
    /// once every node accepted the object, so a partial failure leaves the
    /// manager unchanged and reports the failing nodes.
    #[tracing::instrument(level = "debug", skip(self), fields(network = %network.id))]
    async fn create_network(
        &mut self,
        mut network: Network,
        manager_id: ActorId,
    ) -> Result<Network, String> {
        network.state = None;
        self.admit_network(&network)?;
        if network.provider == NetworkProvider::Routed {
            network.state = Some(NetworkState {
                vni: self.assign_network_vni(&network.id)?,
            });
        }

        let intent = self
            .network_sync
            .publish(manager_id, NetworkUpdate::Ensure(network.clone()));
        let failures = self.network_sync.send(&intent).await;
        if !failures.is_empty() {
            let cleanup = self
                .network_sync
                .publish(manager_id, NetworkUpdate::Remove(network));
            self.network_sync.retire(cleanup);
            return Err(failures.join("; "));
        }

        info!(provider = ?network.provider, vni = ?network.vni(), "created network");
        self.networks.insert(network.id.clone(), network.clone());
        Ok(network)
    }

    /// Rejects a create that cannot be applied as requested.
    fn admit_network(&self, network: &Network) -> Result<(), String> {
        network.validate().map_err(|error| error.to_string())?;
        if self.networks.contains_key(&network.id) {
            return Err(format!("network {} already exists", network.id));
        }
        if self.network_sync.retirement_pending(&network.id) {
            return Err(format!(
                "network {} is still awaiting cleanup acknowledgements",
                network.id
            ));
        }
        require_network_actors(self)
    }

    /// Reserves a VNI for a new routed network.
    fn assign_network_vni(&self, id: &str) -> Result<u32, String> {
        let taken: AHashSet<u32> = self
            .networks
            .values()
            .filter_map(Network::vni)
            .chain(self.network_sync.reserved_vnis())
            .collect();
        assign_vni(id, |candidate| taken.contains(&candidate)).ok_or_else(|| {
            format!(
                "the VNI pool {}-{} is exhausted",
                VNI_RANGE.start(),
                VNI_RANGE.end()
            )
        })
    }

    /// Applies a network removal to every known agent before dropping the entry.
    #[tracing::instrument(level = "debug", skip(self), fields(network = %id))]
    async fn delete_network(&mut self, id: String, manager_id: ActorId) -> Result<(), String> {
        let network = self
            .networks
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("network {id} is not defined"))?;
        require_network_actors(self)?;

        let intent = self
            .network_sync
            .publish(manager_id, NetworkUpdate::Remove(network.clone()));
        let failures = self.network_sync.send(&intent).await;
        if !failures.is_empty() {
            self.network_sync
                .publish(manager_id, NetworkUpdate::Ensure(network));
            return Err(failures.join("; "));
        }

        self.network_sync.retire(intent);
        info!(network = %id, "deleted network");
        self.networks.remove(&id);
        Ok(())
    }
}

/// Deletes a network, refusing while an agent still has NICs on it.
impl Message<DeleteNetwork> for SchedulerActor {
    type Reply = DeleteNetworkReply;

    #[tracing::instrument(level = "debug", skip(self, ctx), fields(network = %msg.id))]
    async fn handle(
        &mut self,
        msg: DeleteNetwork,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match self.delete_network(msg.id, ctx.actor_ref().id()).await {
            Ok(()) => DeleteNetworkReply { error: None },
            Err(error) => DeleteNetworkReply { error: Some(error) },
        }
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
