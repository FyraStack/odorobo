use std::time::Duration;

use ahash::{AHashMap, AHashSet};
use futures_util::{StreamExt, future::join_all};
use kameo::prelude::*;
use tokio::task::JoinHandle;
use tracing::{info, trace, warn};

use crate::messages::network::{GetVtep, NetworkUpdate, SyncNetwork, VtepInfo};
use crate::networking::actor::NetworkAgentActor;
use crate::utils::actor_names::NETWORK;

use super::{CachedActorKind, SchedulerActor};

const NETWORK_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
const NETWORK_SYNC_INTERVAL: Duration = Duration::from_secs(5);
const NETWORK_SYNC_TIMEOUT: Duration = Duration::from_secs(60);
const NETWORK_MAILBOX_TIMEOUT: Duration = Duration::from_secs(5);
const NETWORK_DISCOVERY_CONCURRENCY: usize = 16;

#[derive(Clone, Default)]
struct NodeIdentity {
    node_id: Option<String>,
    fabric: Option<VtepInfo>,
}

struct Target {
    actor_ref: RemoteActorRef<NetworkAgentActor>,
    acknowledged: AHashSet<u64>,
    identity: NodeIdentity,
    attempt: u64,
    task: Option<JoinHandle<()>>,
}

struct Retirement {
    intent: SyncNetwork,
    awaiting: AHashMap<ActorId, NodeIdentity>,
}

// damn this really needs a proper etcd sync instead of
// distributed over libp2p -cappy
// todo: rewrite to watcher after #109
#[derive(Default)]
pub(super) struct NetworkSync {
    revision: u64,
    next_attempt: u64,
    holders: AHashMap<String, AHashMap<ActorId, NodeIdentity>>,
    current: AHashMap<String, SyncNetwork>,
    retired: Vec<Retirement>,
    targets: AHashMap<ActorId, Target>,
    background: Option<JoinHandle<()>>,
}

pub(super) struct NetworkActorDiscovered {
    actor_ref: RemoteActorRef<NetworkAgentActor>,
    identity: NodeIdentity,
}

pub(super) struct ReconcileNetworks;

struct SyncResult {
    intent: SyncNetwork,
    error: Option<String>,
}

struct NetworkSyncFinished {
    actor_id: ActorId,
    attempt: u64,
    results: Vec<SyncResult>,
}

impl NetworkSync {
    pub fn start_background(&mut self, parent: ActorRef<SchedulerActor>) {
        self.background = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(NETWORK_SYNC_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut discovery = tokio::task::JoinSet::new();
            loop {
                interval.tick().await;
                while let Some(result) = discovery.try_join_next() {
                    if let Err(error) = result {
                        warn!(%error, "network discovery task stopped");
                    }
                }
                if discovery.is_empty() {
                    let parent = parent.clone();
                    discovery.spawn(async move {
                        discover_network_actors(&parent).await;
                    });
                }
                if parent.ask(ReconcileNetworks).await.is_err() {
                    return;
                }
            }
        }));
    }

    #[tracing::instrument(level = "debug", skip(self, manager_id, update), fields(network = %update.network().id))]
    pub fn publish(&mut self, manager_id: ActorId, update: NetworkUpdate) -> SyncNetwork {
        trace!("publishing a network update");
        self.revision = self
            .revision
            .checked_add(1)
            .expect("network synchronization revision exhausted");
        let active_owner = match &update {
            NetworkUpdate::Ensure(network) => Some(network.clone()),
            NetworkUpdate::Remove(_) => None,
        };
        let intent = SyncNetwork {
            manager_id,
            revision: self.revision,
            update,
            owners_revision: self.revision,
            active_owner,
        };
        self.current
            .insert(intent.update.network().id.clone(), intent.clone());
        intent
    }

    #[tracing::instrument(level = "debug", skip(self, intent), fields(network = %intent.update.network().id, revision = intent.revision))]
    pub fn retire(&mut self, intent: SyncNetwork) {
        self.current.remove(&intent.update.network().id);
        let awaiting = self
            .holders
            .remove(&intent.update.network().id)
            .unwrap_or_default()
            .into_iter()
            .filter(|(actor_id, identity)| {
                !self.targets.iter().any(|(target_id, target)| {
                    target.acknowledged.contains(&intent.revision)
                        && same_node(*actor_id, identity, *target_id, &target.identity)
                })
            })
            .collect();
        self.retired.push(Retirement { intent, awaiting });
    }

    #[tracing::instrument(level = "debug", skip(self, id), fields(network = %id))]
    pub fn retirement_pending(&self, id: &str) -> bool {
        self.retired.iter().any(|retired| {
            retired.intent.update.network().id == id
                && (!retired.awaiting.is_empty()
                    || self
                        .targets
                        .values()
                        .any(|target| !target.acknowledged.contains(&retired.intent.revision)))
        })
    }

    pub fn reserved_vnis(&self) -> impl Iterator<Item = u32> + '_ {
        self.retired
            .iter()
            .filter(|retired| {
                !retired.awaiting.is_empty()
                    || self
                        .targets
                        .values()
                        .any(|target| !target.acknowledged.contains(&retired.intent.revision))
            })
            .filter_map(|retired| retired.intent.update.network().vni())
    }

    pub fn contains_agent(&self, agent_id: ActorId) -> bool {
        self.targets.keys().any(|id| {
            id.peer_id() == agent_id.peer_id() && self.targets[id].identity.node_id.is_some()
        })
    }

    fn pending(&self, acknowledged: &AHashSet<u64>) -> Vec<SyncNetwork> {
        let mut removals: Vec<_> = self
            .retired
            .iter()
            .map(|retired| &retired.intent)
            .filter(|intent| !acknowledged.contains(&intent.revision))
            .cloned()
            .collect();
        removals.sort_by_key(|intent| intent.revision);
        let mut current: Vec<_> = self
            .current
            .values()
            .filter(|intent| !acknowledged.contains(&intent.revision))
            .cloned()
            .collect();
        current.sort_by_key(|intent| intent.revision);
        removals.extend(current);
        removals.iter().map(|intent| self.prepare(intent)).collect()
    }

    fn prepare(&self, intent: &SyncNetwork) -> SyncNetwork {
        let network = intent.update.network();
        let active_owner = self.current.values().find_map(|current| {
            let NetworkUpdate::Ensure(owner) = &current.update else {
                return None;
            };
            let matches = network.vni().map_or_else(
                || owner.vni().is_none() && owner.id == network.id,
                |vni| owner.vni() == Some(vni),
            );
            matches.then(|| owner.clone())
        });
        SyncNetwork {
            owners_revision: self.revision,
            active_owner,
            ..intent.clone()
        }
    }

    fn remember_holder(&mut self, actor_id: ActorId, intent: &SyncNetwork) {
        if matches!(&intent.update, NetworkUpdate::Ensure(_))
            && let Some(target) = self.targets.get(&actor_id)
        {
            self.holders
                .entry(intent.update.network().id.clone())
                .or_default()
                .insert(actor_id, target.identity.clone());
        }
    }

    fn applicable(&self, intent: &SyncNetwork) -> bool {
        self.current
            .get(&intent.update.network().id)
            .is_some_and(|current| current.same_update(intent))
            || self
                .retired
                .iter()
                .any(|retired| retired.intent.same_update(intent))
    }

    #[tracing::instrument(level = "debug", skip(self, intent), fields(actor_id = %actor_id, revision = intent.revision))]
    fn acknowledge(&mut self, actor_id: ActorId, intent: &SyncNetwork) {
        if !self.applicable(intent) {
            trace!(
                network = %intent.update.network().id,
                "ignored an acknowledgement for a superseded update"
            );
            return;
        }
        let Some(target) = self.targets.get_mut(&actor_id) else {
            trace!("ignored an acknowledgement from an unknown network actor");
            return;
        };
        trace!("acknowledging a network update from a network actor");
        target.acknowledged.insert(intent.revision);
        let identity = target.identity.clone();
        for retired in &mut self.retired {
            if retired.intent.same_update(intent) {
                retired.awaiting.retain(|previous_id, previous_identity| {
                    !same_node(*previous_id, previous_identity, actor_id, &identity)
                });
                if retired.awaiting.is_empty() {
                    trace!(
                        network = %retired.intent.update.network().id,
                        "network retirement waits for no holder"
                    );
                }
            }
        }
    }

    #[tracing::instrument(level = "debug", skip(self, intent), fields(network = %intent.update.network().id, revision = intent.revision))]
    pub async fn send(&mut self, intent: &SyncNetwork) -> Vec<String> {
        let unidentified: Vec<_> = self
            .targets
            .iter()
            .filter(|(_, target)| target.identity.node_id.is_none())
            .map(|(id, _)| format!("{id}: node identity is not available"))
            .collect();
        if !unidentified.is_empty() {
            warn!(
                ?unidentified,
                "network update cannot be sent to all actors because some are unidentified"
            );
            return unidentified;
        }
        trace!("sending a network update to all known actors");
        let intent = self.prepare(intent);
        let targets: Vec<_> = self
            .targets
            .iter()
            .map(|(id, target)| (*id, target.actor_ref.clone()))
            .collect();
        for (actor_id, _) in &targets {
            self.remember_holder(*actor_id, &intent);
        }
        let intent_ref = &intent;
        let results = join_all(targets.into_iter().map(|(actor_id, actor_ref)| async move {
            (actor_id, send_update(&actor_ref, intent_ref).await)
        }))
        .await;
        let mut failures = Vec::new();
        for (actor_id, error) in results {
            if let Some(error) = error {
                failures.push(format!("{actor_id}: {error}"));
            } else {
                self.acknowledge(actor_id, &intent);
            }
        }
        failures
    }

    #[tracing::instrument(level = "debug", skip(self, parent), fields(targets = self.targets.len()))]
    fn start_pending(&mut self, parent: &ActorRef<SchedulerActor>) {
        let plans: Vec<_> = self
            .targets
            .iter()
            .filter(|(_, target)| {
                target.identity.node_id.is_some()
                    && target.task.as_ref().is_none_or(JoinHandle::is_finished)
            })
            .map(|(actor_id, target)| (*actor_id, self.pending(&target.acknowledged)))
            .filter(|(_, pending)| !pending.is_empty())
            .collect();
        for (actor_id, pending) in plans {
            for intent in &pending {
                self.remember_holder(actor_id, intent);
            }
            self.next_attempt = self
                .next_attempt
                .checked_add(1)
                .expect("network synchronization attempt exhausted");
            let attempt = self.next_attempt;
            let target = self
                .targets
                .get_mut(&actor_id)
                .expect("planned network target");
            target.attempt = attempt;
            let actor_ref = target.actor_ref.clone();
            let parent = parent.clone();
            target.task = Some(tokio::spawn(async move {
                let mut results = Vec::new();
                let mut blocked = AHashSet::new();
                for intent in pending {
                    let id = intent.update.network().id.clone();
                    if matches!(&intent.update, NetworkUpdate::Ensure(_)) && blocked.contains(&id) {
                        continue;
                    }
                    let error = send_update(&actor_ref, &intent).await;
                    if error.is_some() && matches!(&intent.update, NetworkUpdate::Remove(_)) {
                        blocked.insert(id);
                    }
                    results.push(SyncResult { intent, error });
                }
                _ = parent
                    .tell(NetworkSyncFinished {
                        actor_id,
                        attempt,
                        results,
                    })
                    .send()
                    .await;
            }));
        }
    }

    #[tracing::instrument(level = "debug", skip(self, identity), fields(actor_id = %actor_id))]
    fn update_identity(&mut self, actor_id: ActorId, identity: &NodeIdentity) {
        if identity.node_id.is_none() && identity.fabric.is_none() {
            return;
        }
        if let Some(target) = self.targets.get_mut(&actor_id) {
            target.identity.clone_from(identity);
        }
        for holders in self.holders.values_mut() {
            for (previous_id, previous_identity) in holders {
                if same_node(*previous_id, previous_identity, actor_id, identity) {
                    previous_identity.clone_from(identity);
                }
            }
        }
        for retired in &mut self.retired {
            for (previous_id, previous_identity) in &mut retired.awaiting {
                if same_node(*previous_id, previous_identity, actor_id, identity) {
                    previous_identity.clone_from(identity);
                }
            }
        }
        let completed: Vec<_> = self
            .retired
            .iter()
            .filter(|retired| {
                self.targets
                    .get(&actor_id)
                    .is_some_and(|target| target.acknowledged.contains(&retired.intent.revision))
            })
            .map(|retired| retired.intent.clone())
            .collect();
        trace!(
            node_id = ?identity.node_id,
            completed = completed.len(),
            holders = self.holders.len(),
            quarantined = self
                .retired
                .iter()
                .filter(|retired| !retired.awaiting.is_empty())
                .count(),
            "applied a network actor identity"
        );
        for intent in completed {
            self.acknowledge(actor_id, &intent);
        }
    }

    #[tracing::instrument(level = "debug", skip(self), fields(actor_id = %actor_id))]
    pub fn remove_target(&mut self, actor_id: ActorId) {
        let quarantined = self
            .holders
            .values()
            .filter(|holders| holders.contains_key(&actor_id))
            .count();
        if let Some(mut target) = self.targets.remove(&actor_id) {
            trace!(
                node_id = ?target.identity.node_id,
                acknowledged = target.acknowledged.len(),
                quarantined,
                "removed a network actor"
            );
            if let Some(task) = target.task.take() {
                task.abort();
            }
        }
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn stop(&mut self) {
        if let Some(task) = self.background.take() {
            task.abort();
            _ = task.await;
        }
        let targets = self.targets.len();
        for (_, mut target) in self.targets.drain() {
            if let Some(task) = target.task.take() {
                task.abort();
                _ = task.await;
            }
        }
        info!(targets, "network synchronization stopped");
    }
}

fn same_node(
    previous_id: ActorId,
    previous_identity: &NodeIdentity,
    actor_id: ActorId,
    identity: &NodeIdentity,
) -> bool {
    if previous_id == actor_id {
        return true;
    }
    if let Some((previous, current)) = previous_identity
        .node_id
        .as_ref()
        .zip(identity.node_id.as_ref())
    {
        return previous == current;
    }
    (previous_id.peer_id().is_some() && previous_id.peer_id() == actor_id.peer_id())
        || previous_identity
            .fabric
            .as_ref()
            .zip(identity.fabric.as_ref())
            .is_some_and(|(previous, current)| {
                previous.vtep == current.vtep
                    && previous.asn == current.asn
                    && previous.underlay.trunc() == current.underlay.trunc()
            })
}

#[tracing::instrument(level = "debug", skip(actor_ref, intent), fields(actor_id = %actor_ref.id(), network = %intent.update.network().id, revision = intent.revision))]
async fn send_update(
    actor_ref: &RemoteActorRef<NetworkAgentActor>,
    intent: &SyncNetwork,
) -> Option<String> {
    let request = actor_ref
        .ask(intent)
        .mailbox_timeout(NETWORK_MAILBOX_TIMEOUT)
        .reply_timeout(NETWORK_SYNC_TIMEOUT)
        .send();
    match tokio::time::timeout(NETWORK_SYNC_TIMEOUT, request).await {
        Ok(Ok(reply)) if reply.revision == intent.revision => reply.error,
        Ok(Ok(_)) => Some("network acknowledgement has a different revision".to_owned()),
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("network synchronization timed out".to_owned()),
    }
}

/// Probes a discovered actor for the node identity used to match holders.
#[tracing::instrument(level = "debug", skip(actor_ref), fields(actor_id = %actor_ref.id()))]
async fn observe_network_actor(
    actor_ref: RemoteActorRef<NetworkAgentActor>,
) -> NetworkActorDiscovered {
    let message = GetVtep;
    let request = actor_ref
        .ask(&message)
        .mailbox_timeout(NETWORK_MAILBOX_TIMEOUT)
        .reply_timeout(NETWORK_MAILBOX_TIMEOUT)
        .send();
    let identity = match tokio::time::timeout(NETWORK_LOOKUP_TIMEOUT, request).await {
        Ok(Ok(reply)) => NodeIdentity {
            node_id: reply.node_id,
            fabric: reply.fabric,
        },
        Ok(Err(error)) => {
            trace!(%error, "network actor did not answer GetVtep");
            NodeIdentity::default()
        }
        Err(_) => {
            trace!("network actor did not answer GetVtep in time");
            NodeIdentity::default()
        }
    };
    NetworkActorDiscovered {
        actor_ref,
        identity,
    }
}

#[tracing::instrument(level = "debug", skip(parent))]
pub(super) async fn discover_network_actors(parent: &ActorRef<SchedulerActor>) {
    let discover = async {
        let mut actors = RemoteActorRef::<NetworkAgentActor>::lookup_all(NETWORK)
            .map(|result| async move {
                let actor_ref = result?;
                Ok::<_, kameo::error::RegistryError>(observe_network_actor(actor_ref).await)
            })
            .buffer_unordered(NETWORK_DISCOVERY_CONCURRENCY);
        while let Some(result) = actors.next().await {
            match result {
                Ok(observation) => {
                    if parent.tell(observation).send().await.is_err() {
                        return;
                    }
                }
                Err(error) => warn!(%error, "network actor discovery failed"),
            }
        }
    };
    if tokio::time::timeout(NETWORK_LOOKUP_TIMEOUT, discover)
        .await
        .is_err()
    {
        warn!("network actor discovery timed out");
    }
}

impl Message<NetworkActorDiscovered> for SchedulerActor {
    type Reply = ();

    #[tracing::instrument(level = "debug", skip(self, msg, ctx), fields(actor_id = %msg.actor_ref.id()))]
    async fn handle(&mut self, msg: NetworkActorDiscovered, ctx: &mut Context<Self, Self::Reply>) {
        let actor_id = msg.actor_ref.id();
        if self.network_sync.targets.contains_key(&actor_id) {
            self.network_sync.update_identity(actor_id, &msg.identity);
            trace!(
                node_id = ?msg.identity.node_id,
                vtep = ?msg.identity.fabric.as_ref().map(|fabric| fabric.vtep),
                "refreshed a known network actor"
            );
            return;
        }
        if let Err(error) = ctx.actor_ref().link_remote(&msg.actor_ref).await {
            warn!(?actor_id, %error, "failed to link network actor");
            return;
        }
        let replaced: Vec<_> = self
            .network_sync
            .targets
            .iter()
            .filter_map(|(previous_id, target)| {
                same_node(*previous_id, &target.identity, actor_id, &msg.identity)
                    .then_some(*previous_id)
            })
            .collect();
        if !replaced.is_empty() {
            info!(?actor_id, replaced = ?replaced, "replaced network actors for the same node");
        }
        for previous_id in replaced {
            self.network_sync.remove_target(previous_id);
            self.actor_kinds.remove(&previous_id);
        }
        trace!(
            ?actor_id,
            node_id = ?msg.identity.node_id,
            vtep = ?msg.identity.fabric.as_ref().map(|fabric| fabric.vtep),
            "registered a network actor"
        );
        self.actor_kinds.insert(actor_id, CachedActorKind::Network);
        self.network_sync.targets.insert(
            actor_id,
            Target {
                actor_ref: msg.actor_ref,
                acknowledged: AHashSet::new(),
                identity: msg.identity,
                attempt: 0,
                task: None,
            },
        );
        self.network_sync.start_pending(ctx.actor_ref());
    }
}

impl Message<ReconcileNetworks> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, _msg: ReconcileNetworks, ctx: &mut Context<Self, Self::Reply>) {
        self.network_sync.start_pending(ctx.actor_ref());
    }
}

impl Message<NetworkSyncFinished> for SchedulerActor {
    type Reply = ();

    #[tracing::instrument(level = "debug", skip(self, msg, _ctx), fields(actor_id = %msg.actor_id, attempt = msg.attempt))]
    async fn handle(&mut self, msg: NetworkSyncFinished, _ctx: &mut Context<Self, Self::Reply>) {
        let Some(target) = self.network_sync.targets.get_mut(&msg.actor_id) else {
            trace!("network synchronization finished for an unknown actor");
            return;
        };
        if target.attempt != msg.attempt {
            trace!(
                attempt = msg.attempt,
                current = target.attempt,
                "discarded a stale synchronization attempt"
            );
            return;
        }
        target.task.take();
        for result in msg.results {
            if !self.network_sync.applicable(&result.intent) {
                trace!(
                    network = %result.intent.update.network().id,
                    revision = result.intent.revision,
                    "discarded a result for a superseded network update"
                );
                continue;
            }
            if let Some(error) = result.error {
                warn!(actor_id = %msg.actor_id, network_id = %result.intent.update.network().id,
                    revision = result.intent.revision, %error, "network synchronization will retry");
            } else {
                self.network_sync.acknowledge(msg.actor_id, &result.intent);
                info!(actor_id = %msg.actor_id, network_id = %result.intent.update.network().id,
                    revision = result.intent.revision, "network update acknowledged");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{Network, NetworkProvider, NetworkState};

    fn network() -> Network {
        Network {
            id: "sync-test".to_owned(),
            subnet: "10.40.10.0/24".parse().unwrap(),
            gateway: None,
            provider: NetworkProvider::Routed,
            state: Some(NetworkState { vni: 1001 }),
            mtu: None,
            bridge: None,
            dhcp: None,
            upstream: None,
        }
    }

    #[test]
    fn only_acknowledged_revisions_leave_the_retry_plan() {
        let mut sync = NetworkSync::default();
        let update = sync.publish(ActorId::new(1), NetworkUpdate::Ensure(network()));
        let mut acknowledged = AHashSet::new();
        assert_eq!(sync.pending(&acknowledged), vec![update.clone()]);
        assert_eq!(sync.pending(&acknowledged), vec![update.clone()]);
        acknowledged.insert(update.revision);
        assert_eq!(sync.pending(&acknowledged), Vec::<SyncNetwork>::new());
    }

    #[test]
    fn retirement_precedes_recreation_and_old_results_cannot_acknowledge_it() {
        let mut sync = NetworkSync::default();
        let original = sync.publish(ActorId::new(1), NetworkUpdate::Ensure(network()));
        let removed = sync.publish(ActorId::new(1), NetworkUpdate::Remove(network()));
        sync.retire(removed.clone());
        let replacement = sync.publish(ActorId::new(1), NetworkUpdate::Ensure(network()));
        assert!(!sync.applicable(&original));
        let plan = sync.pending(&AHashSet::new());
        assert_eq!(plan.len(), 2);
        assert!(plan[0].same_update(&removed));
        assert!(plan[1].same_update(&replacement));
        assert_eq!(
            plan[0].active_owner,
            Some(replacement.update.network().clone())
        );
    }

    #[test]
    fn provider_transition_does_not_assign_a_routed_owner_to_a_bridged_retirement() {
        let mut sync = NetworkSync::default();
        let mut bridged = network();
        bridged.provider = NetworkProvider::Bridged;
        bridged.state = None;
        bridged.bridge = Some("vmbr-test".to_owned());
        let retired = sync.publish(ActorId::new(1), NetworkUpdate::Remove(bridged));
        sync.retire(retired);
        sync.publish(ActorId::new(1), NetworkUpdate::Ensure(network()));
        let plan = sync.pending(&AHashSet::new());
        assert_eq!(plan[0].active_owner, None);
        assert_eq!(plan[1].active_owner, Some(network()));
    }

    #[test]
    fn disconnected_holders_keep_the_vni_quarantined() {
        let mut sync = NetworkSync::default();
        let original = sync.publish(ActorId::new(1), NetworkUpdate::Ensure(network()));
        sync.holders
            .entry(original.update.network().id.clone())
            .or_default()
            .insert(
                ActorId::new(8),
                NodeIdentity {
                    node_id: Some("node-a".to_owned()),
                    fabric: None,
                },
            );
        let removed = sync.publish(ActorId::new(1), NetworkUpdate::Remove(network()));
        sync.retire(removed);
        assert!(sync.retirement_pending("sync-test"));
        assert_eq!(sync.reserved_vnis().collect::<Vec<_>>(), vec![1001]);
        assert_eq!(sync.retired[0].awaiting.len(), 1);
    }

    #[test]
    fn stable_identity_survives_non_evpn_restart_and_enriches_old_holders() {
        let identity = NodeIdentity {
            node_id: Some("node-a".to_owned()),
            fabric: None,
        };
        assert!(same_node(
            ActorId::new(8),
            &identity,
            ActorId::new(9),
            &identity
        ));
        let mut sync = NetworkSync::default();
        sync.holders
            .entry("sync-test".to_owned())
            .or_default()
            .insert(ActorId::new(8), NodeIdentity::default());
        sync.update_identity(ActorId::new(8), &identity);
        assert_eq!(
            sync.holders["sync-test"][&ActorId::new(8)]
                .node_id
                .as_deref(),
            Some("node-a")
        );
    }
}
