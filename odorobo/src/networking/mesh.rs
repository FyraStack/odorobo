//! Off-actor VTEP discovery and actor-owned EVPN peer retention.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use kameo::prelude::{ActorId, RemoteActorRef};
use tokio::time::Instant;
use tracing::{trace, warn};

use super::actor::NetworkAgentActor;
use crate::messages::network::{GetVtep, VtepInfo};
use crate::utils::actor_names::NETWORK;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const RPC_TIMEOUT: Duration = Duration::from_secs(8);
const MAILBOX_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const PASS_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_CONCURRENCY: usize = 16;
const PEER_GRACE: Duration = Duration::from_secs(120);
const EXPIRY_AUTHORIZATION_TTL: Duration = Duration::from_secs(60);
const REQUIRED_PROBE_FAILURES: u8 = 3;
const FAILED_REF_RETRY_DELAY: Duration = Duration::from_secs(300);

#[derive(Debug)]
pub(super) struct PeerObservation {
    pub actor_id: ActorId,
    pub fabric: Option<VtepInfo>,
}

#[derive(Debug)]
pub(super) struct ProbeError {
    pub actor_id: ActorId,
    pub error: String,
}

#[derive(Debug)]
pub(super) struct PeerUpdate {
    pub observed_at: Instant,
    pub observations: Vec<PeerObservation>,
    pub probe_errors: Vec<ProbeError>,
    pub discovery_errors: Vec<String>,
    pub discovery_complete: bool,
    pub probes_complete: bool,
}

struct RetainedActor<T> {
    actor: T,
    last_reply: Instant,
    failed_probes: u8,
    retry_at: Instant,
    fabric: Option<VtepInfo>,
}

impl<T> RetainedActor<T> {
    const fn new(actor: T, now: Instant) -> Self {
        Self {
            actor,
            last_reply: now,
            failed_probes: 0,
            retry_at: now,
            fabric: None,
        }
    }

    const fn answered(&mut self, now: Instant) {
        self.last_reply = now;
        self.failed_probes = 0;
        self.retry_at = now;
    }

    fn failed(&mut self, now: Instant) {
        self.failed_probes = self.failed_probes.saturating_add(1);
        if self.failed_probes >= REQUIRED_PROBE_FAILURES
            && now.saturating_duration_since(self.last_reply) >= PEER_GRACE
        {
            self.retry_at = checked_deadline(now, FAILED_REF_RETRY_DELAY);
        }
    }
}

#[derive(Default)]
pub(super) struct PeerDiscovery {
    known: BTreeMap<ActorId, RetainedActor<RemoteActorRef<NetworkAgentActor>>>,
    next_actor: Option<ActorId>,
}

impl PeerDiscovery {
    #[tracing::instrument(level = "debug", skip(self), fields(?local_actor_id))]
    pub async fn probe(&mut self, local_actor_id: ActorId) -> PeerUpdate {
        let deadline = checked_deadline(Instant::now(), PASS_TIMEOUT);

        let (discovery_errors, discovery_complete) = self.discover(local_actor_id).await;

        let now = Instant::now();
        let actors = self.peers_due(now);
        let order: Vec<_> = actors.iter().map(RemoteActorRef::id).collect();
        let candidates = actors
            .into_iter()
            .map(|actor| (actor.id(), actor))
            .collect();
        let results = probe_candidates(candidates, deadline, query_vtep).await;

        let now = Instant::now();
        let covered = self.apply_results(&results, now);
        self.next_actor = order
            .iter()
            .find(|id| !covered.contains(*id))
            .copied()
            .or_else(|| order.first().copied());

        if !results.complete {
            warn!(
                covered = covered.len(),
                tracked = self.known.len(),
                "EVPN probe pass exceeded its deadline; unfinished probes do not count as failures"
            );
        }
        trace!(
            tracked = self.known.len(),
            resume = ?self.next_actor,
            "completed an EVPN peer probe pass"
        );
        PeerUpdate {
            observed_at: now,
            observations: results.observations,
            probe_errors: results.probe_errors,
            discovery_errors,
            discovery_complete,
            probes_complete: results.complete,
        }
    }

    /// Looks up the network actors and retains the ones not seen before.
    ///
    /// Returns the lookup errors and whether the lookup stream finished.
    #[tracing::instrument(level = "debug", skip(self), fields(?local_actor_id))]
    async fn discover(&mut self, local_actor_id: ActorId) -> (Vec<String>, bool) {
        let mut lookup = RemoteActorRef::<NetworkAgentActor>::lookup_all(NETWORK);
        let discovered = collect_discovery(
            &mut lookup,
            checked_deadline(Instant::now(), LOOKUP_TIMEOUT),
        )
        .await;
        let seen = discovered.actors.len();
        for actor in discovered.actors {
            if actor.id() != local_actor_id {
                self.known
                    .entry(actor.id())
                    .and_modify(|retained| retained.actor = actor.clone())
                    .or_insert_with(|| RetainedActor::new(actor, Instant::now()));
            }
        }
        self.known.remove(&local_actor_id);
        trace!(
            seen,
            tracked = self.known.len(),
            errors = discovered.errors.len(),
            complete = discovered.complete,
            "discovered network actors"
        );
        (discovered.errors, discovered.complete)
    }

    /// Peers whose retry backoff has expired, starting at the resume point.
    fn peers_due(&self, now: Instant) -> Vec<RemoteActorRef<NetworkAgentActor>> {
        let mut actors: Vec<_> = self
            .known
            .values()
            .filter(|retained| retained.retry_at <= now)
            .map(|retained| retained.actor.clone())
            .collect();
        if let Some(next_actor) = self.next_actor {
            let start = actors.partition_point(|actor| actor.id() < next_actor);
            if start < actors.len() {
                actors.rotate_left(start);
            }
        }
        trace!(
            due = actors.len(),
            tracked = self.known.len(),
            resume = ?self.next_actor,
            "selected peers for probing"
        );
        actors
    }

    /// Applies a probe pass to the retained peers and reports which ones it covered.
    fn apply_results(&mut self, results: &ProbeResults, now: Instant) -> BTreeSet<ActorId> {
        for observation in &results.observations {
            if let Some(retained) = self.known.get_mut(&observation.actor_id) {
                retained.answered(now);
                if observation.fabric.is_some() {
                    retained.fabric.clone_from(&observation.fabric);
                }
            }
        }
        for error in &results.probe_errors {
            if let Some(retained) = self.known.get_mut(&error.actor_id) {
                retained.failed(now);
            }
        }
        prune_retained(&mut self.known, now);
        let covered: BTreeSet<_> = results
            .observations
            .iter()
            .map(|observation| observation.actor_id)
            .chain(results.probe_errors.iter().map(|error| error.actor_id))
            .collect();
        trace!(
            answered = results.observations.len(),
            failed = results.probe_errors.len(),
            covered = covered.len(),
            tracked = self.known.len(),
            "applied an EVPN probe pass"
        );
        covered
    }
}

async fn query_vtep(actor: RemoteActorRef<NetworkAgentActor>) -> Result<Option<VtepInfo>, String> {
    actor
        .ask(&GetVtep)
        .mailbox_timeout(MAILBOX_TIMEOUT)
        .reply_timeout(REPLY_TIMEOUT)
        .send()
        .await
        .map(|reply| reply.fabric)
        .map_err(|error| error.to_string())
}

struct ProbeResults {
    observations: Vec<PeerObservation>,
    probe_errors: Vec<ProbeError>,
    complete: bool,
}

async fn probe_candidates<T, F, Fut>(
    actors: Vec<(ActorId, T)>,
    deadline: Instant,
    query: F,
) -> ProbeResults
where
    T: Send,
    F: Fn(T) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<Option<VtepInfo>, String>> + Send,
{
    let query = &query;
    let mut probes = futures_util::stream::iter(actors)
        .map(|(actor_id, actor)| async move {
            match tokio::time::timeout(RPC_TIMEOUT, query(actor)).await {
                Ok(Ok(fabric)) => Ok(PeerObservation { actor_id, fabric }),
                Ok(Err(error)) => Err(ProbeError { actor_id, error }),
                Err(_) => Err(ProbeError {
                    actor_id,
                    error: format!("VTEP probe exceeded {RPC_TIMEOUT:?}"),
                }),
            }
        })
        .buffer_unordered(PROBE_CONCURRENCY);
    let mut result = ProbeResults {
        observations: Vec::new(),
        probe_errors: Vec::new(),
        complete: false,
    };
    loop {
        if Instant::now() >= deadline {
            return result;
        }
        match tokio::time::timeout_at(deadline, probes.next()).await {
            Ok(Some(Ok(observation))) => result.observations.push(observation),
            Ok(Some(Err(error))) => result.probe_errors.push(error),
            Ok(None) => {
                result.complete = true;
                return result;
            }
            Err(_) => return result,
        }
    }
}

struct DiscoveryResults<T> {
    actors: Vec<T>,
    errors: Vec<String>,
    complete: bool,
}

async fn collect_discovery<T, E: std::fmt::Display>(
    stream: &mut (impl Stream<Item = Result<T, E>> + Unpin),
    deadline: Instant,
) -> DiscoveryResults<T> {
    let mut result = DiscoveryResults {
        actors: Vec::new(),
        errors: Vec::new(),
        complete: false,
    };
    loop {
        if Instant::now() >= deadline {
            result
                .errors
                .push("network actor lookup timed out".to_owned());
            return result;
        }
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(actor))) => result.actors.push(actor),
            Ok(Some(Err(error))) => result.errors.push(error.to_string()),
            Ok(None) => {
                result.complete = result.errors.is_empty();
                return result;
            }
            Err(_) => {
                result
                    .errors
                    .push("network actor lookup timed out".to_owned());
                return result;
            }
        }
    }
}

struct Claim {
    last_seen: Instant,
    failed_probes: u8,
}

struct Peer {
    last_seen: Instant,
    claims: BTreeMap<ActorId, Claim>,
}

impl Peer {
    fn expired(&self, now: Instant) -> bool {
        !self.claims.is_empty()
            && now.saturating_duration_since(self.last_seen) >= PEER_GRACE
            && self
                .claims
                .values()
                .all(|claim| claim.failed_probes >= REQUIRED_PROBE_FAILURES)
    }
}

#[derive(Default)]
pub(super) struct MeshPeers {
    peers: BTreeMap<Ipv4Addr, Peer>,
    expiry_authorized_at: Option<Instant>,
}

impl MeshPeers {
    pub fn seed(&mut self, local: &VtepInfo, owned: &BTreeSet<Ipv4Addr>, now: Instant) {
        for vtep in owned {
            if endpoint_valid(local, *vtep) {
                self.peers.entry(*vtep).or_insert_with(|| Peer {
                    last_seen: now,
                    claims: BTreeMap::new(),
                });
            }
        }
    }

    #[tracing::instrument(level = "debug", skip(self, update), fields(vtep = %local.vtep))]
    pub fn observe(&mut self, local: &VtepInfo, update: PeerUpdate) {
        let now = update.observed_at;
        for error in &update.discovery_errors {
            warn!(%error, "EVPN network actor discovery was incomplete");
        }
        for error in &update.probe_errors {
            trace!(actor_id = ?error.actor_id, error = %error.error, "EVPN direct probe failed");
        }
        let mut responses = BTreeMap::new();
        for observation in update.observations {
            let accepted = observation.fabric.filter(|info| {
                let compatible = fabric_compatible(local, info);
                if !compatible { warn!(actor_id = ?observation.actor_id, ?info, "ignoring incompatible EVPN identity"); }
                compatible
            });
            responses.insert(observation.actor_id, accepted.map(|info| info.vtep));
        }
        let failed: BTreeSet<_> = update
            .probe_errors
            .iter()
            .map(|error| error.actor_id)
            .collect();
        for (vtep, peer) in &mut self.peers {
            for (actor_id, claim) in &mut peer.claims {
                match responses.get(actor_id) {
                    Some(Some(reported_vtep)) if reported_vtep == vtep => {
                        claim.last_seen = now;
                        claim.failed_probes = 0;
                        peer.last_seen = now;
                    }
                    Some(_) => claim.failed_probes = claim.failed_probes.saturating_add(1),
                    None if failed.contains(actor_id) => {
                        claim.failed_probes = claim.failed_probes.saturating_add(1);
                    }
                    None => {}
                }
            }
        }
        for (actor_id, vtep) in &responses {
            let Some(vtep) = vtep else {
                continue;
            };
            let peer = self.peers.entry(*vtep).or_insert_with(|| Peer {
                last_seen: now,
                claims: BTreeMap::new(),
            });
            if peer.claims.iter().any(|(other_actor, claim)| {
                other_actor != actor_id
                    && claim.failed_probes == 0
                    && now.saturating_duration_since(claim.last_seen) < PEER_GRACE
            }) {
                warn!(%vtep, ?actor_id, "multiple live network actors claim the same VTEP");
            }
            peer.last_seen = now;
            peer.claims.insert(
                *actor_id,
                Claim {
                    last_seen: now,
                    failed_probes: 0,
                },
            );
        }
        if !update.discovery_complete || !update.probes_complete {
            trace!("partial EVPN discovery pass; unfinished probes do not count as failures");
        }
        self.expiry_authorized_at = responses.values().any(Option::is_some).then_some(now);
        for peer in self.peers.values_mut() {
            if peer.claims.values().any(|claim| {
                claim.failed_probes == 0
                    && now.saturating_duration_since(claim.last_seen) < PEER_GRACE
            }) {
                peer.claims.retain(|_, claim| {
                    claim.failed_probes < REQUIRED_PROBE_FAILURES
                        || now.saturating_duration_since(claim.last_seen) < PEER_GRACE
                });
            }
        }
        self.peers.retain(|vtep, _| endpoint_valid(local, *vtep));
    }

    pub fn desired(&mut self, now: Instant) -> BTreeSet<Ipv4Addr> {
        if self.expiry_authorized_at.is_some_and(|observed_at| {
            now.saturating_duration_since(observed_at) <= EXPIRY_AUTHORIZATION_TTL
        }) {
            self.peers.retain(|vtep, peer| {
                if peer.expired(now) {
                    trace!(%vtep, "expired EVPN peer after failed direct probes and grace");
                    false
                } else {
                    true
                }
            });
        }
        self.peers.keys().copied().collect()
    }
}

fn prune_retained<T>(known: &mut BTreeMap<ActorId, RetainedActor<T>>, now: Instant) {
    let live: Vec<_> = known
        .values()
        .filter(|actor| {
            actor.failed_probes == 0 && now.saturating_duration_since(actor.last_reply) < PEER_GRACE
        })
        .filter_map(|actor| actor.fabric.clone())
        .collect();
    known.retain(|_, actor| {
        if actor.failed_probes < REQUIRED_PROBE_FAILURES
            || now.saturating_duration_since(actor.last_reply) < PEER_GRACE
        {
            return true;
        }
        actor.fabric.as_ref().is_some_and(|old| {
            !live.iter().any(|new| {
                old.vtep == new.vtep
                    && old.asn == new.asn
                    && old.underlay.trunc() == new.underlay.trunc()
            })
        })
    });
}

fn checked_deadline(now: Instant, timeout: Duration) -> Instant {
    now.checked_add(timeout).unwrap_or(now)
}

fn endpoint_valid(local: &VtepInfo, vtep: Ipv4Addr) -> bool {
    local.underlay.contains(&local.vtep) && local.underlay.contains(&vtep) && vtep != local.vtep
}

fn fabric_compatible(local: &VtepInfo, remote: &VtepInfo) -> bool {
    endpoint_valid(local, remote.vtep)
        && local.asn == remote.asn
        && local.underlay.trunc() == remote.underlay.trunc()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(vtep: &str) -> VtepInfo {
        VtepInfo {
            vtep: vtep.parse().unwrap(),
            underlay: "10.77.0.0/24".parse().unwrap(),
            asn: 65000,
        }
    }

    fn update(
        now: Instant,
        observations: &[(u64, Option<VtepInfo>)],
        failures: &[u64],
    ) -> PeerUpdate {
        PeerUpdate {
            observed_at: now,
            observations: observations
                .iter()
                .map(|(id, fabric)| PeerObservation {
                    actor_id: ActorId::new(*id),
                    fabric: fabric.clone(),
                })
                .collect(),
            probe_errors: failures
                .iter()
                .map(|id| ProbeError {
                    actor_id: ActorId::new(*id),
                    error: "unreachable".to_owned(),
                })
                .collect(),
            discovery_errors: Vec::new(),
            discovery_complete: true,
            probes_complete: true,
        }
    }

    #[tokio::test]
    async fn discovery_drains_errors_and_preserves_later_successes() {
        let mut stream = futures_util::stream::iter([Ok(1), Err("bad metadata"), Ok(2)]);
        let result = collect_discovery(&mut stream, Instant::now() + LOOKUP_TIMEOUT).await;
        assert_eq!(result.actors, vec![1, 2]);
        assert_eq!(result.errors, vec!["bad metadata"]);
        assert!(!result.complete);
    }

    #[tokio::test]
    async fn partial_probe_pass_drops_unfinished_queries_without_counting_failures() {
        let actors = (0..48).map(|id| (ActorId::new(id), id)).collect();
        let result = probe_candidates(
            actors,
            Instant::now() + Duration::from_millis(2),
            |id| async move {
                if id == 0 {
                    Ok(Some(info("10.77.0.2")))
                } else {
                    std::future::pending().await
                }
            },
        )
        .await;
        assert!(!result.complete);
        assert_eq!(result.observations.len(), 1);
        assert_eq!(result.probe_errors.len(), 0);
    }

    #[tokio::test]
    async fn probes_run_concurrently_without_exceeding_the_bound() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let actors = (0..48).map(|id| (ActorId::new(id), id)).collect();
        let result = probe_candidates(actors, Instant::now() + PASS_TIMEOUT, |id| {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            async move {
                let count = active.fetch_add(1, Ordering::SeqCst).saturating_add(1);
                maximum.fetch_max(count, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                if id == 1 {
                    Err("unsupported message".to_owned())
                } else {
                    Ok(None)
                }
            }
        })
        .await;
        assert!(result.complete);
        assert_eq!(result.observations.len(), 47);
        assert_eq!(result.probe_errors.len(), 1);
        assert!(maximum.load(Ordering::SeqCst) <= PROBE_CONCURRENCY);
        assert!(maximum.load(Ordering::SeqCst) > 1);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn lookup_omission_and_incomplete_discovery_do_not_remove_a_directly_reachable_peer() {
        let now = Instant::now();
        let local = info("10.77.0.1");
        let remote = info("10.77.0.2");
        let mut peers = MeshPeers::default();
        peers.observe(&local, update(now, &[(2, Some(remote.clone()))], &[]));
        let later = now + PEER_GRACE + Duration::from_secs(1);
        let mut partial = update(later, &[(2, Some(remote.clone()))], &[]);
        partial.discovery_complete = false;
        partial.discovery_errors.push("lookup failed".to_owned());
        peers.observe(&local, partial);
        assert_eq!(peers.desired(later), BTreeSet::from([remote.vtep]));
    }

    #[test]
    fn expiry_requires_grace_failures_and_another_reachable_peer() {
        let now = Instant::now();
        let local = info("10.77.0.1");
        let dead = info("10.77.0.2");
        let live = info("10.77.0.3");
        let mut peers = MeshPeers::default();
        peers.observe(
            &local,
            update(
                now,
                &[(2, Some(dead.clone())), (3, Some(live.clone()))],
                &[],
            ),
        );
        for offset in [10, 20, 30] {
            let at = now + Duration::from_secs(offset);
            peers.observe(&local, update(at, &[(3, Some(live.clone()))], &[2]));
            assert!(peers.desired(at).contains(&dead.vtep));
        }
        let later = now + PEER_GRACE;
        peers.observe(&local, update(later, &[(3, Some(live.clone()))], &[2]));
        assert_eq!(peers.desired(later), BTreeSet::from([live.vtep]));
    }

    #[test]
    fn total_discovery_failure_freezes_expiry() {
        let now = Instant::now();
        let local = info("10.77.0.1");
        let remote = info("10.77.0.2");
        let mut peers = MeshPeers::default();
        peers.observe(&local, update(now, &[(2, Some(remote.clone()))], &[]));
        for offset in [120, 150, 180] {
            let at = now + Duration::from_secs(offset);
            peers.observe(&local, update(at, &[], &[2]));
        }
        assert_eq!(
            peers.desired(now + Duration::from_secs(180)),
            BTreeSet::from([remote.vtep])
        );
    }

    #[test]
    fn replacement_actor_refreshes_one_vtep_despite_old_actor_failures() {
        let now = Instant::now();
        let local = info("10.77.0.1");
        let remote = info("10.77.0.2");
        let mut peers = MeshPeers::default();
        peers.observe(&local, update(now, &[(2, Some(remote.clone()))], &[]));
        for offset in [120, 150, 180] {
            let at = now + Duration::from_secs(offset);
            peers.observe(&local, update(at, &[(3, Some(remote.clone()))], &[2]));
            assert_eq!(peers.desired(at), BTreeSet::from([remote.vtep]));
        }
    }

    #[test]
    fn warm_unidentified_peers_survive_discovery_errors() {
        let now = Instant::now();
        let local = info("10.77.0.1");
        let warm = info("10.77.0.2");
        let live = info("10.77.0.3");
        let mut peers = MeshPeers::default();
        peers.seed(&local, &BTreeSet::from([warm.vtep]), now);
        let later = now + PEER_GRACE + Duration::from_secs(1);
        peers.observe(&local, update(later, &[(3, Some(live.clone()))], &[]));
        assert_eq!(peers.desired(later), BTreeSet::from([warm.vtep, live.vtep]));
    }

    #[test]
    fn completed_probe_evidence_expires_peers_across_partial_passes() {
        let now = Instant::now();
        let local = info("10.77.0.1");
        let dead = info("10.77.0.2");
        let live = info("10.77.0.3");
        let mut peers = MeshPeers::default();
        peers.observe(
            &local,
            update(now, &[(2, Some(dead)), (3, Some(live.clone()))], &[]),
        );
        for offset in [30, 60, 120] {
            let at = now + Duration::from_secs(offset);
            let mut partial = update(at, &[(3, Some(live.clone()))], &[2]);
            partial.discovery_complete = false;
            partial.probes_complete = false;
            peers.observe(&local, partial);
        }
        assert_eq!(peers.desired(now + PEER_GRACE), BTreeSet::from([live.vtep]));
    }

    #[test]
    fn replacement_retires_dead_actor_references_and_old_claims() {
        let now = Instant::now();
        let later = now + PEER_GRACE;
        let remote = info("10.77.0.2");
        let mut old = RetainedActor::new(2, now);
        old.fabric = Some(remote.clone());
        old.failed_probes = 3;
        let mut new = RetainedActor::new(3, later);
        new.fabric = Some(remote.clone());
        let mut known = BTreeMap::from([(ActorId::new(2), old), (ActorId::new(3), new)]);
        prune_retained(&mut known, later);
        assert_eq!(
            known.keys().copied().collect::<Vec<_>>(),
            vec![ActorId::new(3)]
        );
        let local = info("10.77.0.1");
        let mut peers = MeshPeers::default();
        peers.observe(&local, update(now, &[(2, Some(remote.clone()))], &[]));
        for offset in [30, 60, 120] {
            peers.observe(
                &local,
                update(
                    now + Duration::from_secs(offset),
                    &[(3, Some(remote.clone()))],
                    &[2],
                ),
            );
        }
        assert_eq!(peers.peers[&remote.vtep].claims.len(), 1);
    }

    #[test]
    fn compatibility_rejects_self_foreign_asn_and_underlays() {
        let local = info("10.77.0.1");
        assert!(!fabric_compatible(&local, &local));
        assert!(!fabric_compatible(&local, &info("192.0.2.1")));
        let mut wrong_asn = info("10.77.0.2");
        wrong_asn.asn = 65001;
        assert!(!fabric_compatible(&local, &wrong_asn));
        let mut wrong_prefix = info("10.77.0.2");
        wrong_prefix.underlay = "10.77.0.0/25".parse().unwrap();
        assert!(!fabric_compatible(&local, &wrong_prefix));
        let mut normalized = info("10.77.0.2");
        normalized.underlay = "10.77.0.2/24".parse().unwrap();
        assert!(fabric_compatible(&local, &normalized));
    }
}
