//! Cache-only scheduling policy, capacity accounting, and affinity evaluation.

use std::cmp::Ordering;

use ahash::AHashMap;
use kameo::prelude::{ActorId, RemoteActorRef};
use stable_eyre::{Report, eyre::OptionExt};
use tracing::info;
use ulid::Ulid;

use crate::actors::agent_actor::AgentActor;
use crate::manifest::{
    AffinityRequirement, AffinityStrictness, AffinityType, MetadataTable, Operator, VmManifest,
};
use crate::messages::vm::CreateVM;

use super::{CachedAgentActor, SchedulerActor, VmLifecycle, VmPlacement};

const VCPU_OVERPROVISIONMENT_NUMERATOR: u32 = 2;
const VCPU_OVERPROVISIONMENT_DENOMINATOR: u32 = 1;
type MetadataTables<'a> = [(
    &'a std::collections::BTreeMap<String, String>,
    &'a std::collections::BTreeMap<String, String>,
)];

impl SchedulerActor {
    /// Returns resources reserved by `Pending` placements, computing them lazily.
    ///
    /// Running usage comes from cached agent status; only unconfirmed placements
    /// are added here to prevent concurrent create requests from overcommitting.
    pub(super) fn pending_resources(&mut self) -> &AHashMap<ActorId, (u32, u64)> {
        if self.pending_resources_cache.is_none() {
            self.pending_resources_cache = Some(pending_resources_by_agent(
                &self.vm_manifests,
                &self.vm_placements,
            ));
        }
        self.pending_resources_cache
            .as_ref()
            .expect("pending resources cache was just initialized")
    }

    /// Marks pending-placement resource totals stale after a relevant cache change.
    pub(super) fn invalidate_pending_resources(&mut self) {
        self.pending_resources_cache = None;
    }

    /// Determine the best agent to schedule a specific VM creation request to.
    ///
    /// The scheduler first filters agents by capacity and required affinity, then
    /// ranks preferred-affinity totals before CPU/RAM headroom. Equal scores have
    /// no deterministic tie-breaker because the agent cache is a hash map.
    ///
    /// Scoring performs no network I/O and therefore uses an eventually consistent
    /// cache. Affinity rules are roughly based on
    /// <https://kubernetes.io/docs/concepts/scheduling-eviction/assign-pod-node/>.
    pub(super) fn schedule_agent(
        &mut self,
        msg: &CreateVM,
    ) -> Result<RemoteActorRef<AgentActor>, Report> {
        self.pending_resources();
        let pending_resources = self
            .pending_resources_cache
            .as_ref()
            .expect("pending resources cache was just initialized");
        let mut best_agent = None;
        let mut best_score = AgentScore::REJECTED;

        for agent in self.agent_data_cache.values() {
            let score = self.score_agent(msg, agent, pending_resources);

            if score > best_score {
                best_agent = Some(agent.actor_ref.clone());
                best_score = score;
            }
        }

        info!(?best_score, ?best_agent, "best agent");

        best_agent.ok_or_eyre("No valid agents found.")
    }

    #[expect(dead_code, reason = "reserved for a future batch create message")]
    fn schedule_agents(
        &mut self,
        msgs: &[CreateVM],
    ) -> Vec<Result<RemoteActorRef<AgentActor>, Report>> {
        self.pending_resources();
        let pending_resources = self
            .pending_resources_cache
            .as_ref()
            .expect("pending resources cache was just initialized");
        msgs.iter()
            .map(|msg| {
                let mut best_agent = None;
                let mut best_score = AgentScore::REJECTED;
                for agent in self.agent_data_cache.values() {
                    let score = self.score_agent(msg, agent, pending_resources);
                    if score > best_score {
                        best_agent = Some(agent.actor_ref.clone());
                        best_score = score;
                    }
                }
                best_agent.ok_or_eyre("No valid agents found.")
            })
            .collect()
    }

    /// Scores one agent from cached state without performing network I/O.
    ///
    /// vCPUs may use 2× overprovisioning; RAM is not overprovisioned. Pending
    /// placements reserve their requested resources until agent status confirms
    /// them, while running usage comes directly from the cached agent snapshot.
    /// Cache-affecting actions must update cache state before a later affinity
    /// decision depends on them.
    fn score_agent(
        &self,
        msg: &CreateVM,
        agent: &CachedAgentActor,
        pending_resources: &AHashMap<ActorId, (u32, u64)>,
    ) -> AgentScore {
        let mut score = AgentScore::default();

        let agent_max_vcpus = agent
            .data
            .vcpus
            .saturating_mul(VCPU_OVERPROVISIONMENT_NUMERATOR)
            .checked_div(VCPU_OVERPROVISIONMENT_DENOMINATOR)
            .unwrap_or(u32::MAX);
        // todo: do we care about VMData.max_vcpus?
        let (pending_vcpus, pending_ram) = pending_resources
            .get(&agent.actor_ref.id())
            .copied()
            .unwrap_or_default();
        let used_vcpus = agent.data.used_vcpus.saturating_add(pending_vcpus);
        let used_ram = agent.data.used_ram.as_u64().saturating_add(pending_ram);
        let requested_vcpus = msg.config.desired.compute.vcpus;
        let requested_memory = msg.config.desired.compute.memory_bytes;
        let agent_used_vcpus = used_vcpus.saturating_add(requested_vcpus);

        if !has_capacity(
            agent_max_vcpus,
            used_vcpus,
            requested_vcpus,
            agent.data.ram.as_u64(),
            used_ram,
            requested_memory,
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
        let agent_used_ram = bytesize::ByteSize::b(used_ram.saturating_add(requested_memory));

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
        if !msg.config.desired.placement.affinity.is_empty() {
            let affinity_rules = &msg.config.desired.placement.affinity;
            for rule in affinity_rules {
                let mut metadata_tables = Vec::with_capacity(1);
                match rule.affinity_type {
                    AffinityType::VirtualMachine => {
                        metadata_tables.extend(
                            Self::placement_vm_ids(
                                &self.vm_placements,
                                self.agent_vm_index.get(&agent.actor_ref.id()),
                                agent.actor_ref.id(),
                                &agent.data.vms,
                            )
                            .into_iter()
                            .filter_map(|vmid| self.vm_manifests.get(&vmid))
                            .map(|manifest| {
                                (
                                    &manifest.desired.metadata.labels,
                                    &manifest.desired.metadata.annotations,
                                )
                            }),
                        );
                    }
                    AffinityType::Agent => {
                        metadata_tables.push((
                            &agent.data.metadata.labels,
                            &agent.data.metadata.annotations,
                        ));
                    }
                }

                let follows_rule = evaluate_affinity_rule(&metadata_tables, rule);

                let Some(affinity_delta) = affinity_delta(&rule.strictness, follows_rule) else {
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

/// Returns whether a request fits within supplied vCPU and RAM limits.
///
/// Arguments must already include applicable overprovisioning and pending reservations.
pub(super) const fn has_capacity(
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

fn pending_resources_by_agent(
    manifests: &AHashMap<Ulid, VmManifest>,
    placements: &AHashMap<Ulid, Vec<VmPlacement>>,
) -> AHashMap<ActorId, (u32, u64)> {
    let mut resources = AHashMap::new();
    for (vmid, entries) in placements {
        let Some(manifest) = manifests.get(vmid) else {
            continue;
        };
        for entry in entries {
            if entry.lifecycle == VmLifecycle::Pending {
                let totals = resources.entry(entry.agent_id).or_insert((0u32, 0u64));
                totals.0 = totals.0.saturating_add(manifest.desired.compute.vcpus);
                totals.1 = totals
                    .1
                    .saturating_add(manifest.desired.compute.memory_bytes);
            }
        }
    }
    resources
}

#[cfg(test)]
pub(super) fn pending_resources_for_agent(
    manifests: &AHashMap<Ulid, VmManifest>,
    placements: &AHashMap<Ulid, Vec<VmPlacement>>,
    agent_id: ActorId,
) -> (u32, u64) {
    pending_resources_by_agent(manifests, placements)
        .get(&agent_id)
        .copied()
        .unwrap_or_default()
}

/// Converts an affinity result into a score contribution or rejection.
///
/// Failed required rules reject an agent; unmet preferred rules contribute zero.
pub(super) fn affinity_delta(strictness: &AffinityStrictness, follows_rule: bool) -> Option<i64> {
    match strictness {
        AffinityStrictness::Required if !follows_rule => None,
        AffinityStrictness::Required => Some(0),
        AffinityStrictness::Preferred { weight } => {
            Some(i64::from(follows_rule).saturating_mul(*weight))
        }
    }
}

/// Evaluates a rule against the metadata objects in its selected affinity scope.
///
/// Requirements are OR-ed. For a requirement to match, every supplied metadata
/// object must satisfy it; empty metadata therefore does not match. The `anti`
/// direction negates the aggregate result, and a rule without requirements is false
/// before direction is applied.
pub(super) fn evaluate_affinity_rule(
    metadata_tables: &MetadataTables<'_>,
    rule: &crate::manifest::AffinityRule,
) -> bool {
    let mut follows_rule = false;

    for requirement in &rule.requirements {
        let mut requirement_outcome = !metadata_tables.is_empty();

        for object_metadata in metadata_tables {
            let table = match requirement.table {
                MetadataTable::Label => object_metadata.0,
                MetadataTable::Annotation => object_metadata.1,
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

/// Evaluates one metadata value against a requirement.
///
/// Missing keys match only `NotIn`. Numeric `Lt` and `Gt` comparisons require
/// exactly one parseable numeric requirement value; malformed comparisons are false.
pub(super) fn evaluate_table_value(
    value_option: Option<&String>,
    requirement: &AffinityRequirement,
) -> bool {
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
/// Lexicographic scheduling score: affinity takes precedence over resource headroom.
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
