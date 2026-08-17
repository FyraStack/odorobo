use std::cmp::Ordering;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

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
use crate::utils::actor_names::AGENT;
use crate::utils::actor_names::VM;
use crate::utils::actor_names::vm_actor_id;
use ahash::AHashSet;
use dashmap::DashMap;
use dashmap::mapref::multiple::RefMulti;
use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use stable_eyre::eyre::OptionExt;
use stable_eyre::{Report, Result, eyre::eyre};
use tokio::task::JoinHandle;
use tracing::trace;
use tracing::{info, warn};
use ulid::Ulid;

#[derive(Debug, Clone)]
pub struct CachedAgentActor {
    pub actor_ref: RemoteActorRef<AgentActor>,
    pub data: AgentStatus,
    /// this is a set of all VMs that may be on an agent. it is used for rules such as affinity to make sure we don't schedule things in ways that arent allowed
    /// We don't know for a fact these VMs are scheduled due to latency and boot up delay, but they may be scheduled.
    pub extended_vm_set: AHashSet<Ulid>,
}

#[derive(Debug, Clone)]
pub struct CachedVMActor {
    pub actor_ref: Option<RemoteActorRef<VMActor>>,
    /// The agent this VM was scheduled on. Used by the updater task to clean up
    /// the agent's `extended_vm_set` if the VM actor dies before it's linked.
    pub scheduled_agent_id: ActorId,
    pub data: GetVMInfoReply,
}

// todo: i dont like the way this cache is setup. I think we may need to change it later, but it is hard to figure out what the optimal solution is without doing it at least once.
//  especially when we haven't fully made decisions about some other things.
//
// todo: we should improve the cache to not have agents and vms send the full data on every update.
//  I looked at kameo streams to make this better, but they aren't really intended for this kind of long term update use case.
//  They use rust futures::stream which seems to be more intended for you have an iterator for example that will create data, but not like full on sending messages.
//  This could likely be done pretty easily by having two get data messages.
//   Option 1: One that creates a session and sends the full data and then only sends diffs after that.
//   Option 2: we can have one that sends full data, and then another that only sends data that changes.
//  Option 2 is easier to write and uses less compute, but uses more network bandwidth.
#[derive(RemoteActor)]
pub struct SchedulerActor {
    pub agent_data_cache: Arc<DashMap<ActorId, CachedAgentActor>>,
    pub agent_keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,

    // todo: we might need a better way to store this.
    //  we 100% need a way to store vms even if we don't know their actorid (ex: actor hasn't been started or is shutdown)
    //  we also might want to be able to store them without a ulid, possibly
    //  so we might need a vec of vms and then to just store maps/indexes of actorid and ulid to vector index
    //  and then like a freelist or something.
    //  i dont really love that option either though cause it feels overkill.
    //  maybe we sure just be using a proper database entirely?
    //  idk. will figure it out later.
    //
    //  new related problem: i just realized vmid, actorid pairs dont have to be unique.
    //  if a vm is migrating from one actor to another, there might be two actors with the same vmid.
    //
    //  additional context (05/05/2026): we almost may need a way to store them without a ulid, due to how CH migration works.
    //  the question becomes if we want to abstract CH migration away entirely from the scheduler.
    //  we could also possibly ignore it for the non-HA scheduler.
    //  I (caleb) want to ask cappy (and possibly Lea) about these problems.
    //
    //  the best solution for at least some of this is almost certainly having an external reliable DB (such as etcd) to store some of these things permanently.
    //  we will need that specifically for what VMs are supposed to be running, because if a large percentage of the cluster goes down, including the manager, we need a way to recover.
    //  and i dont think leaving that on dashboard which could have high latency is a good idea.
    //  alternatively we could have the other manager nodes try to keep track of that data, but i think we are going to run into issues with keeping the state consistent between all nodes.
    //  we may need to make some architecture designs about db consistency vs uptime vs speed in that situation, and im not doing that on my own.
    pub vm_actorid_ulid_map: Arc<DashMap<ActorId, Ulid>>,
    pub vm_data_cache: Arc<DashMap<Ulid, Vec<CachedVMActor>>>,
    pub vm_keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,

    pub cache_actor_finder: Option<JoinHandle<()>>,
}

// todo: this might need to be a runtime thing but this makes it easy to write for now and could easily be switched out later.
static VCPU_OVERPROVISIONMENT_NUMERATOR: u32 = 2;
static VCPU_OVERPROVISIONMENT_DENOMINATOR: u32 = 1;

impl SchedulerActor {
    #[expect(dead_code, reason = "reserved for explicit placement by actor id")]
    fn lookup_agent_by_actor_id(&self, actor_id: &ActorId) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_data_cache
            .get(actor_id)
            .map(|data| data.actor_ref.clone())
    }

    #[expect(dead_code, reason = "reserved for explicit placement by hostname")]
    fn lookup_agent_by_hostname(&self, hostname: &str) -> Option<RemoteActorRef<AgentActor>> {
        self.agent_data_cache
            .iter()
            .find(|data| data.data.hostname == hostname)
            .map(|data| data.actor_ref.clone())
    }

    // someone should likely give caleb a firm talking to about code duplication due to this section, but things are just different enough that trying to make them one function requires usage of a lot of generics which feels even worse. so i dont know what to do. cappy please fix. i hate this.
    async fn vm_actor_finder(
        parent_actor_ref: RemoteActorRef<Self>,
        vm_actorid_ulid_map: Arc<DashMap<ActorId, Ulid>>,
        data_cache: Arc<DashMap<Ulid, Vec<CachedVMActor>>>,
        keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,
        agent_data_cache: Arc<DashMap<ActorId, CachedAgentActor>>,
    ) -> Result<(), Report> {
        trace!("running vm_actor_finder");

        let mut vm_actor_stream = RemoteActorRef::<VMActor>::lookup_all(VM);

        while let Some(vm_actor) = vm_actor_stream.try_next().await? {
            let vm_actor_id = vm_actor.id();
            let updater_is_running = keepalive_tasks
                .get(&vm_actor_id)
                .is_some_and(|task| !task.is_finished());
            if !updater_is_running {
                keepalive_tasks.remove(&vm_actor_id);
                trace!(?vm_actor, "starting vm_updater_task");

                parent_actor_ref.link_remote(&vm_actor).await?;

                let vm_actorid_ulid_map_clone = Arc::clone(&vm_actorid_ulid_map);
                let data_cache_clone = Arc::clone(&data_cache);
                let agent_data_cache_clone = Arc::clone(&agent_data_cache);
                let updater_task = tokio::spawn(async move {
                    Self::vm_updater_task(
                        vm_actor,
                        vm_actorid_ulid_map_clone,
                        data_cache_clone,
                        agent_data_cache_clone,
                    )
                    .await;
                });

                keepalive_tasks.insert(vm_actor_id, updater_task);
            }
        }

        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the updater keeps polling and performs coordinated cache cleanup"
    )]
    async fn vm_updater_task(
        actor_ref: RemoteActorRef<VMActor>,
        vm_actorid_ulid_map: Arc<DashMap<ActorId, Ulid>>,
        data_cache: Arc<DashMap<Ulid, Vec<CachedVMActor>>>,
        agent_data_cache: Arc<DashMap<ActorId, CachedAgentActor>>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut fails: u8 = 0;
        loop {
            if let Ok(data) = actor_ref.ask(&GetVMInfo { vmid: None }).await {
                let vmid = data.vmid;

                vm_actorid_ulid_map.insert(actor_ref.id(), vmid); // should we be doing this on every loop? idk. but we at least need to do it on the first iteration given we don't know the mapping before that

                let scheduled_agent_id = data_cache
                    .get(&vmid)
                    .and_then(|entries| {
                        entries.iter().find_map(|entry| {
                            entry
                                .actor_ref
                                .as_ref()
                                .is_none_or(|actor| actor.id() == actor_ref.id())
                                .then_some(entry.scheduled_agent_id)
                        })
                    })
                    .unwrap_or_else(|| actor_ref.id());
                let cached_vm = CachedVMActor {
                    actor_ref: Some(actor_ref.clone()),
                    scheduled_agent_id,
                    data,
                };
                data_cache
                    .entry(vmid)
                    .and_modify(|entries| {
                        if let Some(entry) = entries.iter_mut().find(|entry| {
                            entry
                                .actor_ref
                                .as_ref()
                                .is_some_and(|actor| actor.id() == actor_ref.id())
                        }) {
                            *entry = cached_vm.clone();
                        } else if let Some(entry) =
                            entries.iter_mut().find(|entry| entry.actor_ref.is_none())
                        {
                            *entry = cached_vm.clone();
                        } else {
                            entries.push(cached_vm.clone());
                        }
                    })
                    .or_insert_with(|| vec![cached_vm]);

                fails = 0;
            } else {
                fails = fails.saturating_add(1);
            }

            if fails > 5 {
                warn!(
                    ?actor_ref,
                    "can no longer reach vm actor, cleaning up cache entries"
                );

                let vmid = vm_actorid_ulid_map
                    .remove(&actor_ref.id())
                    .map(|(_, vmid)| vmid);

                // if the actor was never reachable, there's no vm_actorid_ulid_map entry.
                // try to recover the vmid from the data_cache by scanning for a stale entry
                // that matches this actor_ref.
                let vmid = vmid.or_else(|| {
                    data_cache.iter().find_map(|entry| {
                        entry
                            .value()
                            .iter()
                            .any(|cached| {
                                cached
                                    .actor_ref
                                    .as_ref()
                                    .is_some_and(|actor| actor.id() == actor_ref.id())
                            })
                            .then(|| *entry.key())
                    })
                });

                if let Some(vmid) = vmid {
                    let removed_cached_vm = data_cache.get_mut(&vmid).and_then(|mut entries| {
                        entries
                            .iter()
                            .position(|entry| {
                                entry
                                    .actor_ref
                                    .as_ref()
                                    .is_some_and(|actor| actor.id() == actor_ref.id())
                            })
                            .map(|index| (index, entries.remove(index)))
                    });
                    if let Some(cached_vm) = removed_cached_vm
                        && data_cache
                            .get(&vmid)
                            .is_none_or(|entries| entries.is_empty())
                    {
                        agent_data_cache.alter(&cached_vm.1.scheduled_agent_id, |_, mut v| {
                            v.extended_vm_set.remove(&vmid);
                            v
                        });
                    }
                    if data_cache
                        .get(&vmid)
                        .is_some_and(|entries| entries.is_empty())
                    {
                        data_cache.remove(&vmid);
                    }
                }

                return;
            }

            interval.tick().await;
        }
    }

    async fn agent_actor_finder(
        parent_actor_ref: RemoteActorRef<Self>,
        data_cache: Arc<DashMap<ActorId, CachedAgentActor>>,
        keepalive_tasks: Arc<DashMap<ActorId, JoinHandle<()>>>,
    ) -> Result<(), Report> {
        trace!("running agent_actor_finder");

        let mut agent_actor_stream = RemoteActorRef::<AgentActor>::lookup_all(AGENT);

        while let Some(agent_actor) = agent_actor_stream.try_next().await? {
            let agent_actor_id = agent_actor.id();
            let updater_is_running = keepalive_tasks
                .get(&agent_actor_id)
                .is_some_and(|task| !task.is_finished());
            if !updater_is_running {
                keepalive_tasks.remove(&agent_actor_id);
                trace!(?agent_actor, "starting agent_updater_task");

                parent_actor_ref.link_remote(&agent_actor).await?;

                let data_cache_clone = Arc::clone(&data_cache);
                let updater_task = tokio::spawn(async move {
                    Self::agent_updater_task(agent_actor, data_cache_clone).await;
                });

                keepalive_tasks.insert(agent_actor_id, updater_task);
            }
        }

        Ok(())
    }

    async fn agent_updater_task(
        actor_ref: RemoteActorRef<AgentActor>,
        data_cache: Arc<DashMap<ActorId, CachedAgentActor>>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut fails: u8 = 0;
        loop {
            if let Ok(data) = actor_ref.ask(&GetAgentStatus).await {
                if data_cache.contains_key(&actor_ref.id()) {
                    data_cache.alter(&actor_ref.id(), |_, mut v| {
                        v.data = data;

                        v.extended_vm_set.retain(|vmid| v.data.vms.contains(vmid));
                        v.extended_vm_set.extend(v.data.vms.iter());

                        v
                    });
                } else {
                    data_cache.insert(
                        actor_ref.id(),
                        CachedAgentActor {
                            actor_ref: actor_ref.clone(),
                            data: data.clone(),
                            extended_vm_set: data.vms.iter().copied().collect(),
                        },
                    );
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
                data_cache.remove(&actor_ref.id());
                return;
            }

            interval.tick().await;
        }
    }

    fn start_actor_finder(&mut self, actor_ref: RemoteActorRef<Self>) {
        let agent_data_cache_arc_clone = Arc::clone(&self.agent_data_cache);
        let agent_keepalive_tasks_arc_clone = Arc::clone(&self.agent_keepalive_tasks);

        let vm_actorid_ulid_map_arc_clone = Arc::clone(&self.vm_actorid_ulid_map);
        let vm_data_cache_arc_clone = Arc::clone(&self.vm_data_cache);
        let vm_keepalive_tasks_arc_clone = Arc::clone(&self.vm_keepalive_tasks);

        self.cache_actor_finder = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                let vm_join_handle = Self::vm_actor_finder(
                    actor_ref.clone(),
                    Arc::clone(&vm_actorid_ulid_map_arc_clone),
                    Arc::clone(&vm_data_cache_arc_clone),
                    Arc::clone(&vm_keepalive_tasks_arc_clone),
                    Arc::clone(&agent_data_cache_arc_clone),
                );

                let agent_join_handle = Self::agent_actor_finder(
                    actor_ref.clone(),
                    Arc::clone(&agent_data_cache_arc_clone),
                    Arc::clone(&agent_keepalive_tasks_arc_clone),
                );

                // intentionally ignoring results because we want to keep finding actors even if an attempt fails
                let (vm_result, agent_result) = tokio::join!(vm_join_handle, agent_join_handle);
                if let Err(error) = vm_result {
                    warn!(?error, "VM actor discovery failed");
                }
                if let Err(error) = agent_result {
                    warn!(?error, "agent actor discovery failed");
                }

                //info!(?vm_data_cache_arc_clone);
                //info!(?agent_data_cache_arc_clone);

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

        for agent in self.agent_data_cache.iter() {
            let score = self.score_agent(msg, &agent);

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
    fn score_agent(
        &self,
        msg: &CreateVM,
        agent: &RefMulti<'_, ActorId, CachedAgentActor>,
    ) -> AgentScore {
        let mut score = AgentScore::default();

        let agent_max_vcpus = agent
            .data
            .vcpus
            .saturating_mul(VCPU_OVERPROVISIONMENT_NUMERATOR)
            .checked_div(VCPU_OVERPROVISIONMENT_DENOMINATOR)
            .unwrap_or(u32::MAX);
        // todo: do we care about VMData.max_vcpus?
        let agent_used_vcpus = agent.data.used_vcpus.saturating_add(msg.config.data.vcpus);

        if agent_used_vcpus >= agent_max_vcpus {
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

        // todo: add ram overprovisionment.     not adding this to scheduler until it works on the hypervisor side.
        let agent_max_ram = agent.data.ram;
        let agent_used_ram = bytesize::ByteSize::b(
            agent
                .data
                .used_ram
                .as_u64()
                .saturating_add(msg.config.data.memory.as_u64()),
        );

        if agent_used_ram >= agent_max_ram {
            return AgentScore::REJECTED;
        }

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
                        for vmid in &agent.extended_vm_set {
                            let Some(vm_data_cache_refs) = self.vm_data_cache.get(vmid) else {
                                continue;
                            };

                            for vm_data_cache_ref in vm_data_cache_refs.iter() {
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

                let mut follows_rule = false;

                for requirement in &rule.requirements {
                    let mut requirement_outcome = true;

                    for object_metadata in &metadata_tables {
                        let table = match requirement.table {
                            MetadataTable::Label => &object_metadata.labels,
                            MetadataTable::Annotation => &object_metadata.annotations,
                        };

                        let value_option = table.get(&requirement.key);

                        if !evaluate_table_value(value_option, requirement) {
                            requirement_outcome = false;
                            break;
                        }
                    }

                    if requirement_outcome {
                        follows_rule = true;
                        break;
                    }
                }

                follows_rule ^= rule.inverse;

                match (rule.strictness, follows_rule) {
                    (AffinityStrictness::Required, false) => return AgentScore::REJECTED,
                    (AffinityStrictness::Required, true) => {} // specifically do nothing
                    (AffinityStrictness::Preferred { weight }, follows_rule) => {
                        let follows_rule = i64::from(follows_rule);
                        score.affinity = score
                            .affinity
                            .saturating_add(follows_rule.saturating_mul(weight));
                    }
                }
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

fn evaluate_table_value(value_option: Option<&String>, requirement: &AffinityRequirement) -> bool {
    let Some(value) = value_option else {
        return false;
    };

    match requirement.operator {
        Operator::In => requirement.values.contains(value),
        Operator::NotIn => !requirement.values.contains(value),
        Operator::Lt | Operator::Gt => {
            if requirement.values.len() != 1 {
                return false;
            }

            let Ok(value_number): Result<f64, _> = value.parse() else {
                return false;
            };

            let Ok(requirement_value_number): Result<f64, _> = requirement.values[0].parse() else {
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
    use super::evaluate_table_value;
    use crate::types::{AffinityRequirement, MetadataTable, Operator};
    use std::collections::BTreeMap;

    fn requirement(operator: Operator, values: &[&str]) -> AffinityRequirement {
        AffinityRequirement {
            key: "tier".to_owned(),
            table: MetadataTable::Label,
            operator,
            values: values.iter().map(|value| (*value).to_owned()).collect(),
        }
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
            agent_data_cache: Arc::new(DashMap::new()),
            agent_keepalive_tasks: Arc::new(DashMap::new()),
            vm_actorid_ulid_map: Arc::new(DashMap::new()),
            vm_data_cache: Arc::new(DashMap::new()),
            vm_keepalive_tasks: Arc::new(DashMap::new()),
            cache_actor_finder: None,
        };

        scheduler_actor.start_actor_finder(actor_ref.into_remote_ref().await);

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

        if let Some((_, keepalive_task)) = self.agent_keepalive_tasks.remove(&id) {
            trace!(?id, "Aborting agent keepalive task");
            keepalive_task.abort();
        }

        self.agent_data_cache.remove(&id);

        if let Some((_, keepalive_task)) = self.vm_keepalive_tasks.remove(&id) {
            trace!(?id, "Aborting vm keepalive task");
            keepalive_task.abort();
        }

        if let Some((_, vmid)) = self.vm_actorid_ulid_map.remove(&id)
            && let Some(mut entries) = self.vm_data_cache.get_mut(&vmid)
        {
            let removed_cached_vm = entries
                .iter()
                .position(|entry| {
                    entry
                        .actor_ref
                        .as_ref()
                        .is_some_and(|actor| actor.id() == id)
                })
                .map(|index| entries.remove(index));
            if entries.is_empty() {
                drop(entries);
                self.vm_data_cache.remove(&vmid);
                if let Some(cached_vm) = removed_cached_vm
                    && let Some(mut agent) =
                        self.agent_data_cache.get_mut(&cached_vm.scheduled_agent_id)
                {
                    agent.extended_vm_set.remove(&vmid);
                }
            }
        }

        // todo: attempt vm restarts if necessary.

        Ok(ControlFlow::Continue(()))
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

        // we add to cache first, because we want to make sure future requests assume this vm exists. if the message fails, we clean it up afterward.
        if let Some(mut cached_data) = self.agent_data_cache.get_mut(&target_agent.id()) {
            cached_data.extended_vm_set.insert(msg.vmid);
        } else {
            return Err(eyre!("target agent is not in data cache"));
        }

        self.vm_data_cache
            .entry(msg.vmid)
            .or_default()
            .push(CachedVMActor {
                actor_ref: None,
                scheduled_agent_id: target_agent.id(),
                data: GetVMInfoReply {
                    vmid: msg.vmid,
                    config: Some(msg.config.clone()),
                },
            });

        let reply = target_agent.ask(&msg).await;

        if reply.is_err() {
            // A lost reply is not a rejected create. Keep the cache if the VM actor exists.
            let actor_exists = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid))
                .await?
                .is_some();
            if !actor_exists {
                self.agent_data_cache.alter(&target_agent.id(), |_, mut v| {
                    v.extended_vm_set.remove(&msg.vmid);
                    v
                });
                if let Some(mut entries) = self.vm_data_cache.get_mut(&msg.vmid) {
                    entries.retain(|entry| entry.actor_ref.is_some());
                    if entries.is_empty() {
                        drop(entries);
                        self.vm_data_cache.remove(&msg.vmid);
                    }
                }
            }
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

        for agent in self.agent_data_cache.iter() {
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
