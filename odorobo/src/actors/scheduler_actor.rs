use std::cmp::Ordering;

use std::ops::ControlFlow;

use std::time::{Duration, Instant};

use crate::actors::agent_actor::AgentActor;
use crate::ch_driver::actor::VMActor;
use crate::messages::agent::{AgentStatus, GetAgentStatus};
use crate::messages::vm::{
    AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply, GetVMInfo,
    GetVMInfoReply, ShutdownVM, ShutdownVMReply,
};
use crate::messages::{Ping, Pong};
use crate::types::AffinityRequirement;
use crate::types::AffinityStrictness;
use crate::types::AffinityType;
use crate::types::MetadataTable;
use crate::types::ObjectMetadata;
use crate::types::Operator;
use crate::types::VirtualMachine;
use crate::utils::actor_names::AGENT;
use crate::utils::actor_names::VM;
use crate::utils::actor_names::vm_actor_id;
use ahash::{AHashMap, AHashSet};
use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use stable_eyre::eyre::OptionExt;
use stable_eyre::{Report, Result, eyre::eyre};
use tokio::task::JoinHandle;
use tracing::trace;
use tracing::{info, warn};
use ulid::Ulid;

#[derive(Debug)]
struct VmActorDiscovered {
    actor_ref: RemoteActorRef<VMActor>,
}

#[derive(Debug)]
struct AgentActorDiscovered {
    actor_ref: RemoteActorRef<AgentActor>,
}

#[derive(Debug)]
struct VmUpdated {
    actor_ref: RemoteActorRef<VMActor>,
    data: GetVMInfoReply,
}

#[derive(Debug)]
struct VmUpdaterStopped {
    actor_id: ActorId,
}

#[derive(Debug)]
struct AgentUpdated {
    actor_id: ActorId,
    actor_ref: RemoteActorRef<AgentActor>,
    data: AgentStatus,
}

#[derive(Debug)]
struct AgentUpdaterStopped {
    actor_id: ActorId,
}

#[derive(Debug)]
struct ReconcileVmPlacements;

#[derive(Debug, Clone)]
pub struct CachedAgentActor {
    pub actor_ref: RemoteActorRef<AgentActor>,
    pub data: AgentStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmLifecycle {
    Pending,
    Running,
}

#[derive(Debug, Clone)]
pub struct VmPlacement {
    pub agent_id: ActorId,
    pub config: VirtualMachine,
    pub lifecycle: VmLifecycle,
    pub created_at: Instant,
    pub last_confirmed_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct CachedVMActor {
    pub actor_ref: Option<RemoteActorRef<VMActor>>,
    pub data: GetVMInfoReply,
    pub cached_at: Instant,
}

// todo: we should improve the cache to not have agents and vms send the full data on every update.
//  I looked at kameo streams to make this better, but they aren't really intended for this kind of long term update use case.
//  They use rust futures::stream which seems to be more intended for you have an iterator for example that will create data, but not like full on sending messages.
//  This could likely be done pretty easily by having two get data messages.
//   Option 1: One that creates a session and sends the full data and then only sends diffs after that.
//   Option 2: we can have one that sends full data, and then another that only sends data that changes.
//  Option 2 is easier to write and uses less compute, but uses more network bandwidth.
#[derive(RemoteActor)]
pub struct SchedulerActor {
    pub agent_data_cache: AHashMap<ActorId, CachedAgentActor>,
    pub agent_keepalive_tasks: AHashMap<ActorId, JoinHandle<()>>,

    pub vm_actorid_ulid_map: AHashMap<ActorId, Ulid>,
    /// A VM may be placed on multiple agents while it is migrating.
    pub vm_placements: AHashMap<Ulid, Vec<VmPlacement>>,
    /// this is a vec because a vmid/ulid can be scheduled on multiple boxes simultaneously during migration
    pub vm_data_cache: AHashMap<Ulid, Vec<CachedVMActor>>,
    pub vm_keepalive_tasks: AHashMap<ActorId, JoinHandle<()>>,

    pub cache_actor_finder: Option<JoinHandle<()>>,
}

// todo: this might need to be a runtime thing but this makes it easy to write for now and could easily be switched out later.
static VCPU_OVERPROVISIONMENT_NUMERATOR: u32 = 2;
static VCPU_OVERPROVISIONMENT_DENOMINATOR: u32 = 1;
const UNRESOLVED_VM_CACHE_TIMEOUT: Duration = Duration::from_secs(30);

impl SchedulerActor {
    fn update_cached_vm_entry(
        entries: &mut Vec<CachedVMActor>,
        actor_id: ActorId,
        cached_vm: CachedVMActor,
    ) {
        if let Some(entry) = entries.iter_mut().find(|entry| {
            entry
                .actor_ref
                .as_ref()
                .is_some_and(|actor| actor.id() == actor_id)
        }) {
            *entry = cached_vm;
        } else if let Some(entry) = entries.iter_mut().find(|entry| entry.actor_ref.is_none()) {
            *entry = cached_vm;
        } else {
            entries.push(cached_vm);
        }
    }

    fn cleanup_unresolved_vm_cache(
        placements: &mut AHashMap<Ulid, Vec<VmPlacement>>,
        data_cache: &mut AHashMap<Ulid, Vec<CachedVMActor>>,
    ) {
        let now = Instant::now();
        let empty_vmids: Vec<_> = placements
            .iter_mut()
            .filter_map(|(vmid, entries)| {
                entries.retain(|entry| {
                    entry.lifecycle != VmLifecycle::Pending
                        || now.duration_since(entry.created_at) < UNRESOLVED_VM_CACHE_TIMEOUT
                });
                entries.is_empty().then_some(*vmid)
            })
            .collect();

        for vmid in empty_vmids {
            Self::remove_vm_state(vmid, placements, data_cache);
        }
    }

    /// takes a list of observed vmids and gives you a set of every vmid that could be on an agent.
    fn placement_vm_ids(
        placements: &AHashMap<Ulid, Vec<VmPlacement>>,
        agent_id: ActorId,
        observed: &[Ulid],
    ) -> AHashSet<Ulid> {
        observed
            .iter()
            .copied()
            .chain(placements.iter().filter_map(|(vmid, entries)| {
                entries
                    .iter()
                    .any(|entry| entry.agent_id == agent_id)
                    .then_some(*vmid)
            }))
            .collect()
    }

    fn remove_vm_state(
        vmid: Ulid,
        placements: &mut AHashMap<Ulid, Vec<VmPlacement>>,
        data_cache: &mut AHashMap<Ulid, Vec<CachedVMActor>>,
    ) {
        placements.remove(&vmid);
        data_cache.remove(&vmid);
    }

    fn remove_vm_actor(actor_id: ActorId, data_cache: &mut AHashMap<Ulid, Vec<CachedVMActor>>) {
        let empty_vmids: Vec<_> = data_cache
            .iter_mut()
            .filter_map(|(vmid, entries)| {
                entries.retain(|entry| {
                    entry
                        .actor_ref
                        .as_ref()
                        .is_none_or(|actor| actor.id() != actor_id)
                });
                entries.is_empty().then_some(*vmid)
            })
            .collect();
        for vmid in empty_vmids {
            data_cache.remove(&vmid);
        }
    }

    fn reconcile_agent_placements(
        agent_id: ActorId,
        status: &AgentStatus,
        placements: &mut AHashMap<Ulid, Vec<VmPlacement>>,
    ) {
        let now = Instant::now();
        let observed: AHashSet<_> = status.vms.iter().copied().collect();
        let missing_agent_placements: Vec<_> = observed
            .iter()
            .filter_map(|vmid| {
                let entries = placements.get(vmid)?;
                (!entries.iter().any(|entry| entry.agent_id == agent_id))
                    .then(|| (*vmid, entries[0].config.clone()))
            })
            .collect();
        for (vmid, config) in missing_agent_placements {
            placements.entry(vmid).or_default().push(VmPlacement {
                agent_id,
                config,
                lifecycle: VmLifecycle::Running,
                created_at: now,
                last_confirmed_at: Some(now),
            });
        }

        let empty_vmids: Vec<_> = placements
            .iter_mut()
            .filter_map(|(vmid, entries)| {
                entries.retain(|entry| {
                    entry.agent_id != agent_id
                        || entry.lifecycle == VmLifecycle::Pending
                        || observed.contains(vmid)
                });
                for entry in entries
                    .iter_mut()
                    .filter(|entry| entry.agent_id == agent_id)
                {
                    if observed.contains(vmid) {
                        entry.lifecycle = VmLifecycle::Running;
                        entry.last_confirmed_at = Some(now);
                    }
                }
                entries.is_empty().then_some(*vmid)
            })
            .collect();
        for vmid in empty_vmids {
            placements.remove(&vmid);
        }
    }

    #[expect(dead_code, reason = "reserved for explicit placement by actor id")]
    fn lookup_agent_by_actor_id(&self, actor_id: &ActorId) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_data_cache
            .get(actor_id)
            .map(|data| data.actor_ref.clone())
    }

    #[expect(dead_code, reason = "reserved for explicit placement by hostname")]
    fn lookup_agent_by_hostname(&self, hostname: &str) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_data_cache
            .values()
            .find(|data| data.data.hostname == hostname)
            .map(|data| data.actor_ref.clone())
    }

    async fn vm_actor_finder(parent_actor_ref: ActorRef<Self>) -> Result<(), Report> {
        trace!("running vm_actor_finder");

        let mut vm_actor_stream = RemoteActorRef::<VMActor>::lookup_all(VM);

        while let Some(vm_actor) = vm_actor_stream.try_next().await? {
            parent_actor_ref
                .tell(VmActorDiscovered {
                    actor_ref: vm_actor,
                })
                .send()
                .await?;
        }

        Ok(())
    }

    async fn vm_updater_task(scheduler: ActorRef<Self>, actor_ref: RemoteActorRef<VMActor>) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut fails: u8 = 0;
        loop {
            if let Ok(data) = actor_ref.ask(&GetVMInfo { vmid: None }).await {
                let send_result = scheduler
                    .tell(VmUpdated {
                        actor_ref: actor_ref.clone(),
                        data,
                    })
                    .send()
                    .await
                    .map_err(|error| eyre!("failed to send VM update: {error}"));
                if let Err(error) = send_result {
                    warn!(?error, "VM updater could not notify scheduler");
                    return;
                }

                fails = 0;
            } else {
                fails = fails.saturating_add(1);
            }

            if fails > 5 {
                warn!(
                    ?actor_ref,
                    "can no longer reach vm actor, cleaning up cache entries"
                );

                let send_result = scheduler
                    .tell(VmUpdaterStopped {
                        actor_id: actor_ref.id(),
                    })
                    .send()
                    .await
                    .map_err(|error| eyre!("failed to send VM stop: {error}"));
                if let Err(error) = send_result {
                    warn!(?error, "VM updater could not notify scheduler");
                }
                return;
            }

            interval.tick().await;
        }
    }

    async fn agent_actor_finder(parent_actor_ref: ActorRef<Self>) -> Result<(), Report> {
        trace!("running agent_actor_finder");

        let mut agent_actor_stream = RemoteActorRef::<AgentActor>::lookup_all(AGENT);

        while let Some(agent_actor) = agent_actor_stream.try_next().await? {
            parent_actor_ref
                .tell(AgentActorDiscovered {
                    actor_ref: agent_actor,
                })
                .send()
                .await?;
        }

        Ok(())
    }

    async fn agent_updater_task(scheduler: ActorRef<Self>, actor_ref: RemoteActorRef<AgentActor>) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut fails: u8 = 0;
        loop {
            if let Ok(data) = actor_ref.ask(&GetAgentStatus).await {
                let send_result = scheduler
                    .tell(AgentUpdated {
                        actor_id: actor_ref.id(),
                        actor_ref: actor_ref.clone(),
                        data,
                    })
                    .send()
                    .await
                    .map_err(|error| eyre!("failed to send agent update: {error}"));
                if let Err(error) = send_result {
                    warn!(?error, "agent updater could not notify scheduler");
                    return;
                }
                fails = 0;
            } else {
                fails = fails.saturating_add(1);
            }

            if fails > 5 {
                warn!(
                    ?actor_ref,
                    "can no longer reach agent actor, stopping updater"
                );
                let send_result = scheduler
                    .tell(AgentUpdaterStopped {
                        actor_id: actor_ref.id(),
                    })
                    .send()
                    .await
                    .map_err(|error| eyre!("failed to send agent stop: {error}"));
                if let Err(error) = send_result {
                    warn!(?error, "agent updater could not notify scheduler");
                }
                return;
            }

            interval.tick().await;
        }
    }

    fn start_actor_finder(&mut self, actor_ref: ActorRef<Self>) {
        self.cache_actor_finder = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                let vm_result = Self::vm_actor_finder(actor_ref.clone()).await;
                let agent_result = Self::agent_actor_finder(actor_ref.clone()).await;
                if let Err(error) = vm_result {
                    warn!(?error, "VM actor discovery failed");
                }
                if let Err(error) = agent_result {
                    warn!(?error, "agent actor discovery failed");
                }
                actor_ref.tell(ReconcileVmPlacements).send().await.ok();
                interval.tick().await;
            }
        }));
    }

    /// Determine the best agent to schedule a specific VM creation request to.
    ///
    /// Rough explanation of the algorithm:
    ///     Loop through every known agent.
    ///         Go through a set of rules to determine if the VM can be scheduled on this agent at all, and an affinity score and a general score.
    ///
    ///     Based on these scores, pick the best agent.
    ///         First the affinity score is used, because these are things the customer specifically wanted.
    ///         If the affinity score is tied, we use the general score as a tie breaker.
    ///         The general score uses things like resource utilization to not over load any specific agent.
    ///
    ///
    /// Affinity rules are roughly based on <https://kubernetes.io/docs/concepts/scheduling-eviction/assign-pod-node/>.
    ///
    /// todo:
    ///  - the cache likely needs to be updated automatically when a new vm is scheduled for info like used resources, because otherwise we have to deal with latency on that data we are using
    ///    and then if someone tries to schedule lets say 10 VMs in a batch, we could end up scheduling them all to the same agent because the metadata hasn't updated.
    ///    - there are a few solutions for this but they all kinda suck, mostly due to also making sure we deal with latency properly. I am ignoring the issue for now.
    fn schedule_agent(&self, msg: &CreateVM) -> Result<RemoteActorRef<AgentActor>, Report> {
        let mut best_agent = None;
        let mut best_score = AgentScore::REJECTED;

        for agent in self.agent_data_cache.values() {
            let score = self.score_agent(msg, agent);

            if score > best_score {
                best_agent = Some(agent.actor_ref.clone());
                best_score = score;
            }
        }

        info!(?best_score, ?best_agent, "best agent");

        best_agent.ok_or_eyre("No valid agents found.")
    }

    // this function intentionally only checks against the cache. this has some positives and negatives:
    // positive: it will never trigger any network requests so its very fast, and having to do network requests for scoring whenever we want to schedule a vm is likely a bad idea
    // negative: it technically has a delayed view of the cluster, meaning that some things that happened in the future, may not exist yet. so we need to be careful about how this is done so affinity rules are not accidentally broken. mostly this means, if we do anything that could affect the outcome of an affinity rule (ex: network request to an agent), we need to update the cache, before we do the action.
    fn score_agent(&self, msg: &CreateVM, agent: &CachedAgentActor) -> AgentScore {
        let mut score = AgentScore::default();

        let agent_max_vcpus = agent
            .data
            .vcpus
            .saturating_mul(VCPU_OVERPROVISIONMENT_NUMERATOR)
            .checked_div(VCPU_OVERPROVISIONMENT_DENOMINATOR)
            .unwrap_or(u32::MAX);
        // todo: do we care about VMData.max_vcpus?
        let (pending_vcpus, pending_ram) =
            pending_resources_for_agent(&self.vm_placements, agent.actor_ref.id());
        let used_vcpus = agent.data.used_vcpus.saturating_add(pending_vcpus);
        let used_ram = agent.data.used_ram.as_u64().saturating_add(pending_ram);
        let agent_used_vcpus = used_vcpus.saturating_add(msg.config.data.vcpus);

        if !has_capacity(
            agent_max_vcpus,
            used_vcpus,
            msg.config.data.vcpus,
            agent.data.ram.as_u64(),
            used_ram,
            msg.config.data.memory.as_u64(),
        ) {
            return AgentScore::REJECTED;
        }

        #[expect(
            clippy::cast_precision_loss,
            reason = "the scheduler score intentionally uses f32 ratios"
        )]
        #[expect(
            clippy::arithmetic_side_effects,
            reason = "the preceding capacity check guarantees non-negative subtraction"
        )]
        let vcpu_headroom = (agent_max_vcpus - agent_used_vcpus) as f32 / agent_max_vcpus as f32;
        score.general += vcpu_headroom;

        // todo: add ram overprovisionment. not adding this to scheduler until it works on the hypervisor side.
        let agent_max_ram = agent.data.ram;
        let agent_used_ram =
            bytesize::ByteSize::b(used_ram.saturating_add(msg.config.data.memory.as_u64()));

        #[expect(
            clippy::cast_precision_loss,
            reason = "the scheduler score intentionally uses f32 ratios"
        )]
        let ram_headroom = agent_max_ram
            .as_u64()
            .saturating_sub(agent_used_ram.as_u64()) as f32
            / agent_max_ram.as_u64() as f32;
        score.general += ram_headroom;

        // Roughly based on <https://kubernetes.io/docs/concepts/scheduling-eviction/assign-pod-node/>.
        if let Some(affinity_rules) = &msg.config.affinity {
            for rule in affinity_rules {
                let mut metadata_tables: Vec<ObjectMetadata> = Vec::with_capacity(1);

                match rule.affinity_type {
                    AffinityType::VirtualMachine => {
                        let vmids = Self::placement_vm_ids(
                            &self.vm_placements,
                            agent.actor_ref.id(),
                            &agent.data.vms,
                        );
                        for vmid in vmids {
                            let Some(vm_data_cache_refs) = self.vm_data_cache.get(&vmid) else {
                                continue;
                            };

                            for vm_data_cache_ref in vm_data_cache_refs {
                                let Some(vm_manifest) = &vm_data_cache_ref.data.config else {
                                    continue;
                                };
                                if let Some(metadata) = &vm_manifest.metadata {
                                    metadata_tables.push(metadata.clone());
                                }
                            }
                        }
                    }
                    AffinityType::Agent => metadata_tables.push(agent.data.metadata.clone()),
                }

                let follows_rule = evaluate_affinity_rule(&metadata_tables, rule);

                let Some(affinity_delta) = affinity_delta(rule.strictness, follows_rule) else {
                    return AgentScore::REJECTED;
                };
                score.affinity = score.affinity.saturating_add(affinity_delta);
            }
        }

        // todo (future): possibly keep a percent of agents completely empty, to be able to be converted to dedis automatically.
        // they would have their agent score set to like f32::MIN, so they can be scheduled to if there is no other available agents.
        // rough pseudo code to implement this:
        // if agent.metadata.vms.len() == 0 && hash(agent.config.hostname) % total_chance < threshold {
        //     agent_score = 1;
        // }

        score
    }
}

const fn has_capacity(
    max_vcpus: u32,
    used_vcpus: u32,
    requested_vcpus: u32,
    max_ram: u64,
    used_ram: u64,
    requested_ram: u64,
) -> bool {
    used_vcpus.saturating_add(requested_vcpus) <= max_vcpus
        && used_ram.saturating_add(requested_ram) <= max_ram
}

fn pending_resources_for_agent(
    placements: &AHashMap<Ulid, Vec<VmPlacement>>,
    agent_id: ActorId,
) -> (u32, u64) {
    placements
        .values()
        .flat_map(|entries| entries.iter())
        .filter(|entry| entry.agent_id == agent_id && entry.lifecycle == VmLifecycle::Pending)
        .map(|entry| (entry.config.data.vcpus, entry.config.data.memory.as_u64()))
        .fold((0u32, 0u64), |(vcpus, ram), (vm_vcpus, vm_ram)| {
            (vcpus.saturating_add(vm_vcpus), ram.saturating_add(vm_ram))
        })
}

fn affinity_delta(strictness: AffinityStrictness, follows_rule: bool) -> Option<i64> {
    match strictness {
        AffinityStrictness::Required if !follows_rule => None,
        AffinityStrictness::Required => Some(0),
        AffinityStrictness::Preferred { weight } => {
            Some(i64::from(follows_rule).saturating_mul(weight))
        }
    }
}

fn evaluate_affinity_rule(
    metadata_tables: &[ObjectMetadata],
    rule: &crate::types::AffinityRule,
) -> bool {
    let mut follows_rule = false;

    for requirement in &rule.requirements {
        let mut requirement_outcome = !metadata_tables.is_empty();

        for object_metadata in metadata_tables {
            let table = match requirement.table {
                MetadataTable::Label => &object_metadata.labels,
                MetadataTable::Annotation => &object_metadata.annotations,
            };

            if !evaluate_table_value(table.get(&requirement.key), requirement) {
                requirement_outcome = false;
                break;
            }
        }

        if requirement_outcome {
            follows_rule = true;
            break;
        }
    }

    follows_rule ^ rule.inverse
}

fn evaluate_table_value(value_option: Option<&String>, requirement: &AffinityRequirement) -> bool {
    let Some(value) = value_option else {
        return matches!(requirement.operator, Operator::NotIn);
    };

    match requirement.operator {
        Operator::In => requirement.values.contains(value),
        Operator::NotIn => !requirement.values.contains(value),
        Operator::Lt | Operator::Gt => {
            let [requirement_value] = &requirement.values[..] else {
                return false;
            };

            let Ok(value_number): Result<f64, _> = value.parse() else {
                return false;
            };

            let Ok(requirement_value_number): Result<f64, _> = requirement_value.parse() else {
                return false;
            };

            if requirement.operator == Operator::Lt {
                value_number < requirement_value_number
            } else {
                value_number > requirement_value_number
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CachedVMActor, SchedulerActor, VmLifecycle, VmPlacement, affinity_delta,
        evaluate_affinity_rule, evaluate_table_value, has_capacity, pending_resources_for_agent,
    };

    use crate::messages::agent::AgentStatus;
    use crate::types::{
        AffinityRequirement, AffinityRule, AffinityStrictness, AffinityType, MetadataTable,
        ObjectMetadata, Operator, VirtualMachine,
    };
    use ahash::AHashMap;
    use bytesize::ByteSize;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};
    use ulid::Ulid;

    fn requirement(operator: Operator, values: &[&str]) -> AffinityRequirement {
        AffinityRequirement {
            key: "tier".to_owned(),
            table: MetadataTable::Label,
            operator,
            values: values.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    #[test]
    fn removes_expired_unresolved_vm_placeholders() {
        let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
        let agent_id = super::ActorId::new(1);
        let mut placements: AHashMap<Ulid, Vec<VmPlacement>> = AHashMap::new();
        placements.insert(
            vmid,
            vec![VmPlacement {
                agent_id,
                config: VirtualMachine::default(),
                lifecycle: VmLifecycle::Pending,
                created_at: Instant::now()
                    .checked_sub(Duration::from_secs(31))
                    .expect("test timestamp should be representable"),
                last_confirmed_at: None,
            }],
        );
        let mut data_cache: AHashMap<Ulid, Vec<CachedVMActor>> = AHashMap::new();

        SchedulerActor::cleanup_unresolved_vm_cache(&mut placements, &mut data_cache);

        assert!(!placements.contains_key(&vmid));
        assert!(!data_cache.contains_key(&vmid));
    }

    #[test]
    fn reserves_pending_vm_resources_until_agent_status_confirms_them() {
        let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
        let agent_id = super::ActorId::new(1);
        let mut config = VirtualMachine::default();
        config.data.vcpus = 4;
        config.data.memory = ByteSize::gib(8);
        let mut placements: AHashMap<Ulid, Vec<VmPlacement>> = AHashMap::new();
        placements.insert(
            vmid,
            vec![VmPlacement {
                agent_id,
                config,
                lifecycle: VmLifecycle::Pending,
                created_at: Instant::now(),
                last_confirmed_at: None,
            }],
        );

        assert_eq!(
            pending_resources_for_agent(&placements, agent_id),
            (4, ByteSize::gib(8).as_u64())
        );
    }

    #[test]
    fn reconciling_source_agent_preserves_destination_migration_placement() {
        let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
        let source_agent = super::ActorId::new(1);
        let destination_agent = super::ActorId::new(2);
        let running_placement = |agent_id| VmPlacement {
            agent_id,
            config: VirtualMachine::default(),
            lifecycle: VmLifecycle::Running,
            created_at: Instant::now(),
            last_confirmed_at: Some(Instant::now()),
        };
        let mut placements = AHashMap::from([(vmid, vec![running_placement(source_agent)])]);
        let destination_status = AgentStatus {
            hostname: "destination".to_owned(),
            vcpus: 1,
            ram: ByteSize::b(1),
            used_vcpus: 0,
            used_ram: ByteSize::b(0),
            vms: vec![vmid],
            metadata: ObjectMetadata::default(),
        };
        SchedulerActor::reconcile_agent_placements(
            destination_agent,
            &destination_status,
            &mut placements,
        );

        let source_status = AgentStatus {
            hostname: "source".to_owned(),
            vcpus: 1,
            ram: ByteSize::b(1),
            used_vcpus: 0,
            used_ram: ByteSize::b(0),
            vms: Vec::new(),
            metadata: ObjectMetadata::default(),
        };

        SchedulerActor::reconcile_agent_placements(source_agent, &source_status, &mut placements);

        let remaining = placements
            .get(&vmid)
            .expect("destination placement remains");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].agent_id, destination_agent);
    }

    #[test]
    fn evaluates_membership_and_missing_keys() {
        let metadata = BTreeMap::from([("tier".to_owned(), "frontend".to_owned())]);
        assert!(evaluate_table_value(
            metadata.get("tier"),
            &requirement(Operator::In, &["frontend", "api"])
        ));
        assert!(!evaluate_table_value(
            metadata.get("tier"),
            &requirement(Operator::In, &["backend"])
        ));
        assert!(!evaluate_table_value(
            None,
            &requirement(Operator::In, &["frontend"])
        ));
        assert!(evaluate_table_value(
            None,
            &requirement(Operator::NotIn, &["frontend"])
        ));
    }

    #[test]
    fn evaluates_not_in_and_numeric_comparisons() {
        let metadata = BTreeMap::from([("tier".to_owned(), "4".to_owned())]);
        assert!(evaluate_table_value(
            metadata.get("tier"),
            &requirement(Operator::NotIn, &["5"])
        ));
        assert!(evaluate_table_value(
            metadata.get("tier"),
            &requirement(Operator::Lt, &["5"])
        ));
        assert!(evaluate_table_value(
            metadata.get("tier"),
            &requirement(Operator::Gt, &["3"])
        ));
        assert!(!evaluate_table_value(
            metadata.get("tier"),
            &requirement(Operator::Lt, &["4", "5"])
        ));
        assert!(!evaluate_table_value(
            metadata.get("tier"),
            &requirement(Operator::Gt, &["not-a-number"])
        ));
    }

    #[test]
    fn evaluates_inverse_and_empty_requirements() {
        let metadata = ObjectMetadata {
            labels: BTreeMap::from([("tier".to_owned(), "frontend".to_owned())]),
            annotations: BTreeMap::new(),
        };
        let rule = AffinityRule {
            strictness: AffinityStrictness::Required,
            affinity_type: AffinityType::Agent,
            inverse: true,
            requirements: vec![requirement(Operator::In, &["frontend"])],
        };
        assert!(!evaluate_affinity_rule(
            std::slice::from_ref(&metadata),
            &rule
        ));

        let empty_rule = AffinityRule {
            strictness: AffinityStrictness::Required,
            affinity_type: AffinityType::Agent,
            inverse: false,
            requirements: Vec::new(),
        };
        assert!(!evaluate_affinity_rule(&[], &empty_rule));
    }

    #[test]
    fn evaluates_required_preferred_and_capacity_rules() {
        assert_eq!(affinity_delta(AffinityStrictness::Required, true), Some(0));
        assert_eq!(affinity_delta(AffinityStrictness::Required, false), None);
        assert_eq!(
            affinity_delta(AffinityStrictness::Preferred { weight: 7 }, true),
            Some(7)
        );
        assert_eq!(
            affinity_delta(AffinityStrictness::Preferred { weight: 7 }, false),
            Some(0)
        );
        assert!(has_capacity(8, 2, 2, 16, 4, 4));
        assert!(has_capacity(8, 6, 2, 16, 4, 4));
        assert!(has_capacity(8, 2, 2, 16, 12, 4));
        assert!(!has_capacity(8, 7, 2, 16, 4, 4));
        assert!(!has_capacity(8, 2, 2, 16, 13, 4));
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct AgentScore {
    general: f32,
    affinity: i64,
}

impl AgentScore {
    pub const REJECTED: Self = Self {
        general: f32::NEG_INFINITY,
        affinity: i64::MIN,
    };
}

impl Default for AgentScore {
    fn default() -> Self {
        Self {
            general: 0.0,
            affinity: 0,
        }
    }
}

impl PartialOrd for AgentScore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let affinity_cmp = self.affinity.cmp(&other.affinity);

        if affinity_cmp != Ordering::Equal {
            return Some(affinity_cmp);
        }

        self.general.partial_cmp(&other.general)
    }
}

impl Actor for SchedulerActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let peer_id = *actor_ref.id().peer_id().unwrap();

        info!(?peer_id, "Scheduler Actor started!");

        let mut scheduler_actor = Self {
            agent_data_cache: AHashMap::new(),
            agent_keepalive_tasks: AHashMap::new(),
            vm_actorid_ulid_map: AHashMap::new(),
            vm_placements: AHashMap::new(),
            vm_data_cache: AHashMap::new(),
            vm_keepalive_tasks: AHashMap::new(),
            cache_actor_finder: None,
        };

        scheduler_actor.start_actor_finder(actor_ref);

        Ok(scheduler_actor)
    }

    async fn on_link_died(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        warn!(?id, ?reason, "Linked actor died");

        // check that scheduler actor is still alive.
        let Some(_) = actor_ref.upgrade() else {
            return Ok(ControlFlow::Break(ActorStopReason::Killed));
        };

        if let Some(keepalive_task) = self.agent_keepalive_tasks.remove(&id) {
            trace!(?id, "Aborting agent keepalive task");
            keepalive_task.abort();
        }

        self.agent_data_cache.remove(&id);

        if let Some(keepalive_task) = self.vm_keepalive_tasks.remove(&id) {
            trace!(?id, "Aborting vm keepalive task");
            keepalive_task.abort();
        }

        self.vm_actorid_ulid_map.remove(&id);
        Self::remove_vm_actor(id, &mut self.vm_data_cache);

        // todo: attempt vm restarts if necessary.

        Ok(ControlFlow::Continue(()))
    }
}

impl Message<VmActorDiscovered> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: VmActorDiscovered, ctx: &mut Context<Self, Self::Reply>) {
        let actor_id = msg.actor_ref.id();
        let updater_is_running = self
            .vm_keepalive_tasks
            .get(&actor_id)
            .is_some_and(|task| !task.is_finished());
        if updater_is_running {
            return;
        }
        self.vm_keepalive_tasks.remove(&actor_id);
        if let Err(error) = ctx.actor_ref().link_remote(&msg.actor_ref).await {
            warn!(?error, ?actor_id, "failed to link VM actor");
            return;
        }
        let scheduler = ctx.actor_ref().clone();
        let actor_ref = msg.actor_ref;
        let task = tokio::spawn(async move {
            Self::vm_updater_task(scheduler, actor_ref).await;
        });
        self.vm_keepalive_tasks.insert(actor_id, task);
    }
}

impl Message<AgentActorDiscovered> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: AgentActorDiscovered, ctx: &mut Context<Self, Self::Reply>) {
        let actor_id = msg.actor_ref.id();
        let updater_is_running = self
            .agent_keepalive_tasks
            .get(&actor_id)
            .is_some_and(|task| !task.is_finished());
        if updater_is_running {
            return;
        }
        self.agent_keepalive_tasks.remove(&actor_id);
        if let Err(error) = ctx.actor_ref().link_remote(&msg.actor_ref).await {
            warn!(?error, ?actor_id, "failed to link agent actor");
            return;
        }
        let scheduler = ctx.actor_ref().clone();
        let actor_ref = msg.actor_ref;
        let task = tokio::spawn(async move {
            Self::agent_updater_task(scheduler, actor_ref).await;
        });
        self.agent_keepalive_tasks.insert(actor_id, task);
    }
}

impl Message<VmUpdated> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: VmUpdated, _ctx: &mut Context<Self, Self::Reply>) {
        let vmid = msg.data.vmid;
        let actor_id = msg.actor_ref.id();
        self.vm_actorid_ulid_map.insert(actor_id, vmid);
        let cached_vm = CachedVMActor {
            actor_ref: Some(msg.actor_ref),
            data: msg.data,
            cached_at: Instant::now(),
        };
        let entries = self.vm_data_cache.entry(vmid).or_default();
        Self::update_cached_vm_entry(entries, actor_id, cached_vm);
    }
}

impl Message<VmUpdaterStopped> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: VmUpdaterStopped, _ctx: &mut Context<Self, Self::Reply>) {
        self.vm_keepalive_tasks.remove(&msg.actor_id);
        self.vm_actorid_ulid_map.remove(&msg.actor_id);
        Self::remove_vm_actor(msg.actor_id, &mut self.vm_data_cache);
    }
}

impl Message<AgentUpdated> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: AgentUpdated, _ctx: &mut Context<Self, Self::Reply>) {
        Self::reconcile_agent_placements(msg.actor_id, &msg.data, &mut self.vm_placements);
        self.agent_data_cache.insert(
            msg.actor_id,
            CachedAgentActor {
                actor_ref: msg.actor_ref,
                data: msg.data,
            },
        );
    }
}

impl Message<AgentUpdaterStopped> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: AgentUpdaterStopped, _ctx: &mut Context<Self, Self::Reply>) {
        self.agent_keepalive_tasks.remove(&msg.actor_id);
        self.agent_data_cache.remove(&msg.actor_id);
    }
}

impl Message<ReconcileVmPlacements> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, _msg: ReconcileVmPlacements, _ctx: &mut Context<Self, Self::Reply>) {
        Self::cleanup_unresolved_vm_cache(&mut self.vm_placements, &mut self.vm_data_cache);
    }
}

impl Message<CreateVM> for SchedulerActor {
    type Reply = Result<CreateVMReply, Report>;

    async fn handle(
        &mut self,
        msg: CreateVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let target_agent = self.schedule_agent(&msg)?;

        self.vm_placements
            .entry(msg.vmid)
            .or_default()
            .push(VmPlacement {
                agent_id: target_agent.id(),
                config: msg.config.clone(),
                lifecycle: VmLifecycle::Pending,
                created_at: Instant::now(),
                last_confirmed_at: None,
            });
        self.vm_data_cache
            .entry(msg.vmid)
            .or_default()
            .push(CachedVMActor {
                actor_ref: None,
                data: GetVMInfoReply {
                    vmid: msg.vmid,
                    config: Some(msg.config.clone()),
                },
                cached_at: Instant::now(),
            });

        let reply = target_agent.ask(&msg).await;

        if let Ok(reply) = &reply
            && let Some(actor_id_bytes) = &reply.actor_id
            && let Ok(actor_id) = ActorId::from_bytes(actor_id_bytes)
        {
            self.vm_actorid_ulid_map.insert(actor_id, msg.vmid);
        }

        if reply.is_err()
            && RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid))
                .await
                .ok()
                .flatten()
                .is_none()
        {
            Self::remove_vm_state(msg.vmid, &mut self.vm_placements, &mut self.vm_data_cache);
        }

        Ok(reply?)
    }
}

impl Message<DeleteVM> for SchedulerActor {
    type Reply = Result<DeleteVMReply, Report>;

    async fn handle(
        &mut self,
        msg: DeleteVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?;
        tracing::trace!(?vm, "DeleteVM");
        if let Some(vm) = vm {
            // don't update cache, because we rely on link dying and updater task to remove from cache once the VM is fully down.
            vm.tell(&msg).send()?;
            Ok(DeleteVMReply)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

impl Message<ShutdownVM> for SchedulerActor {
    type Reply = Result<ShutdownVMReply, Report>;

    async fn handle(
        &mut self,
        msg: ShutdownVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?;
        tracing::trace!(?vm, "ShutdownVM");
        if let Some(vm) = vm {
            // don't update cache, because we rely on link dying and updater task to remove from cache once the VM is fully down.
            vm.tell(&msg).send()?;
            Ok(ShutdownVMReply)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

/// this only gets data from the cache from agents
/// we may need a different message that actually forcibly runs/updates everything.
/// and/or messages that get data directly from the `VMActors`.
impl Message<AgentListVMs> for SchedulerActor {
    type Reply = Result<AgentListVMsReply, Report>;

    async fn handle(
        &mut self,
        _msg: AgentListVMs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut vms = Vec::new();

        for agent in self.agent_data_cache.values() {
            vms.extend_from_slice(agent.data.vms.as_slice());
        }

        Ok(AgentListVMsReply { vms })
    }
}

impl Message<Ping> for SchedulerActor {
    type Reply = Pong;

    async fn handle(&mut self, _msg: Ping, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        Pong
    }
}
