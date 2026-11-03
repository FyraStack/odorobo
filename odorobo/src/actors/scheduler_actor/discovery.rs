//! Remote actor discovery, polling tasks, and their internal messages.

use std::time::{Duration, Instant};

use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use stable_eyre::{Report, eyre::eyre};
use tracing::{info, trace, warn};

use crate::actors::agent_actor::AgentActor;
use crate::ch_driver::actor::VMActor;
use crate::messages::agent::{AgentStatusUpdate, GetAgentStatus, apply_status_update};
use crate::messages::vm::{DeleteVM, DeleteVMReply, GetVMHeartbeat, GetVMInfo};
use crate::utils::actor_names::{AGENT, VM};

use super::deletion::{AUTO_DELETE_TIMEOUT, status_update_held_vmids};
use super::{
    AgentActorDiscovered, AgentUpdated, AgentUpdaterStopped, AutoDeleteFinished, CachedActorKind,
    CachedAgentActor, CachedVMActor, ReconcileVmPlacements, SchedulerActor, VmActorDiscovered,
    VmDeleteOwner, VmUpdated, VmUpdaterStopped,
};

impl SchedulerActor {
    /// Enumerates currently discoverable VM actors and forwards each to the scheduler.
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

    /// Resolves a VM's identity once, then heartbeats it every second.
    ///
    /// Six consecutive failed requests stop the task and request cache cleanup.
    async fn vm_updater_task(scheduler: ActorRef<Self>, actor_ref: RemoteActorRef<VMActor>) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut fails: u8 = 0;
        let mut initialized = false;

        loop {
            if !initialized {
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
                    initialized = true;
                    fails = 0;
                } else {
                    fails = fails.saturating_add(1);
                }
            } else if actor_ref.ask(&GetVMHeartbeat).await.is_ok() {
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

    /// Enumerates currently discoverable agents and forwards each to the scheduler.
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

    /// Polls revisioned agent status every second and forwards accepted responses.
    ///
    /// The task begins with revision zero so its first response establishes a
    /// full snapshot. Six consecutive failed requests stop it and request cleanup.
    async fn agent_updater_task(scheduler: ActorRef<Self>, actor_ref: RemoteActorRef<AgentActor>) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut status_revision = 0;
        let mut initial_status = true;
        let mut fails: u8 = 0;
        loop {
            if let Ok(update) = actor_ref
                .ask(&GetAgentStatus {
                    since_revision: status_revision,
                    initial: initial_status,
                })
                .await
            {
                status_revision = match &update {
                    AgentStatusUpdate::Full { revision, .. }
                    | AgentStatusUpdate::Delta { revision, .. } => *revision,
                };
                initial_status = false;
                let send_result = scheduler
                    .tell(AgentUpdated {
                        actor_id: actor_ref.id(),
                        actor_ref: actor_ref.clone(),
                        update,
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

    /// Starts the periodic discovery and pending-placement maintenance loop.
    ///
    /// Each five-second pass discovers both actor types, then asks the scheduler
    /// to expire unresolved pending placements.
    pub(super) fn start_actor_finder(&mut self, actor_ref: ActorRef<Self>) {
        self.cache_actor_finder = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
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
}

/// Registers a discovered VM actor and starts its single polling task.
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
        self.actor_kinds.insert(actor_id, CachedActorKind::Vm);
        let scheduler = ctx.actor_ref().clone();
        let actor_ref = msg.actor_ref;
        let task = tokio::spawn(async move {
            Self::vm_updater_task(scheduler, actor_ref).await;
        });
        self.vm_keepalive_tasks.insert(actor_id, task);
    }
}

/// Registers a discovered agent and starts its single status-polling task.
impl Message<AgentActorDiscovered> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: AgentActorDiscovered, ctx: &mut Context<Self, Self::Reply>) {
        let actor_id = msg.actor_ref.id();
        self.remember_agent_ref(&msg.actor_ref);
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
        self.actor_kinds.insert(actor_id, CachedActorKind::Agent);
        let scheduler = ctx.actor_ref().clone();
        let actor_ref = msg.actor_ref;
        let task = tokio::spawn(async move {
            Self::agent_updater_task(scheduler, actor_ref).await;
        });
        self.agent_keepalive_tasks.insert(actor_id, task);
    }
}

/// Caches a VM's canonical ID, manifest when supplied, and discovered actor reference.
impl Message<VmUpdated> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: VmUpdated, _ctx: &mut Context<Self, Self::Reply>) {
        let vmid = msg.data.vmid;
        // if a delete is still pending, late discovery should still tell us about a
        // cleanup owner we didn't know about, but shouldn't bring back placement intent
        if self.should_record_delete_owner(vmid) {
            self.remember_vm_delete_owner(vmid, VmDeleteOwner::vm(msg.actor_ref.id()));
        }
        if self.vm_tombstones.contains_key(&vmid) {
            return;
        }
        let actor_id = msg.actor_ref.id();
        self.vm_actorid_ulid_map.insert(actor_id, vmid);
        if let Some(manifest) = msg.data.config {
            self.vm_effective_manifests.insert(vmid, manifest.clone());
            // if we only found this manifest after the scheduler started, it's the
            // only intent we've got. otherwise keep the original request. effective
            // values like an agent-assigned vsock CID are node-local and shouldn't
            // turn into hard requirements during failover
            self.vm_manifests.entry(vmid).or_insert(manifest);
            Self::reconcile_discovered_vm(
                vmid,
                &self.agent_vm_index,
                &self.vm_manifests,
                &mut self.vm_placements,
            );
            self.invalidate_pending_resources();
        }
        let cached_vm = CachedVMActor {
            actor_ref: Some(msg.actor_ref),
        };
        let entries = self.vm_data_cache.entry(vmid).or_default();
        Self::update_cached_vm_entry(entries, actor_id, cached_vm);
    }
}

/// Removes cache state after a VM updater determines its actor is unreachable.
impl Message<VmUpdaterStopped> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: VmUpdaterStopped, _ctx: &mut Context<Self, Self::Reply>) {
        self.actor_kinds.remove(&msg.actor_id);
        self.cleanup_vm_actor(msg.actor_id);
    }
}

impl SchedulerActor {
    /// applies an agent status update to the caches. returns whether it was
    /// accepted, so callers know the cached snapshot is fresh. a same-revision
    /// delta with no membership changes is accepted too, since it carries the
    /// latest usage and `retained_vms` and background deletes need those to retry
    fn apply_agent_update(&mut self, msg: AgentUpdated) -> bool {
        let cached_revision = self
            .agent_data_cache
            .get(&msg.actor_id)
            .map(|cached| cached.status_revision);
        let Some(held) = status_update_held_vmids(&msg.update, cached_revision) else {
            return false;
        };
        self.remember_agent_ref(&msg.actor_ref);
        self.record_status_delete_owners(msg.actor_id, held);
        let Some(cached) = self.agent_data_cache.get_mut(&msg.actor_id) else {
            if let AgentStatusUpdate::Full { revision, status } = msg.update {
                self.agent_vm_index
                    .insert(msg.actor_id, status.vms.iter().copied().collect());
                self.invalidate_pending_resources();
                Self::reconcile_agent_placements(
                    msg.actor_id,
                    &status,
                    &self.vm_manifests,
                    &mut self.vm_placements,
                );
                self.invalidate_pending_resources();
                self.agent_data_cache.insert(
                    msg.actor_id,
                    CachedAgentActor {
                        actor_ref: msg.actor_ref,
                        data: status,
                        status_revision: revision,
                    },
                );
                return true;
            }
            return false;
        };

        if let AgentStatusUpdate::Delta { added, removed, .. } = &msg.update {
            let added = added.clone();
            let removed = removed.clone();
            cached.status_revision = apply_status_update(&mut cached.data, msg.update);
            cached.actor_ref = msg.actor_ref;
            self.agent_vm_index
                .entry(msg.actor_id)
                .or_default()
                .extend(added.iter().copied());
            if let Some(index) = self.agent_vm_index.get_mut(&msg.actor_id) {
                for vmid in &removed {
                    index.remove(vmid);
                }
            }
            self.invalidate_pending_resources();
            Self::reconcile_agent_delta(
                msg.actor_id,
                &added,
                &removed,
                &self.vm_manifests,
                &mut self.vm_placements,
            );
        } else {
            cached.status_revision = apply_status_update(&mut cached.data, msg.update);
            cached.actor_ref = msg.actor_ref;
            self.agent_vm_index
                .insert(msg.actor_id, cached.data.vms.iter().copied().collect());
            Self::reconcile_agent_placements(
                msg.actor_id,
                &cached.data,
                &self.vm_manifests,
                &mut self.vm_placements,
            );
            self.invalidate_pending_resources();
        }
        true
    }

    /// sends a background delete for every VM this agent still reports (running
    /// or retained) that we already finished deleting, like after a force delete
    /// skipped a node that later came back. stopped VMs are never touched. the
    /// status handler doesn't wait on these, results come back as `AutoDeleteFinished`
    fn spawn_auto_deletes(&mut self, scheduler: &ActorRef<Self>, agent_id: ActorId) {
        let Some(cached) = self.agent_data_cache.get(&agent_id) else {
            return;
        };
        let agent = cached.actor_ref.clone();
        let reported: Vec<_> = cached
            .data
            .vms
            .iter()
            .chain(&cached.data.retained_vms)
            .copied()
            .collect();
        // the cached snapshot is complete even after a delta, so anything it
        // doesn't list isn't on this node anymore
        self.forget_unreported_leftovers(agent_id, &reported.iter().copied().collect());
        for vmid in self.take_auto_delete_vmids(agent_id, reported, Instant::now()) {
            info!(%vmid, ?agent_id, "agent still holds a deleted VM, deleting it in the background");
            let scheduler = scheduler.clone();
            let agent = agent.clone();
            let request = DeleteVM {
                vmid,
                force: false,
                issued_at: Some(self.next_stamp()),
            };
            tokio::spawn(async move {
                let error =
                    match tokio::time::timeout(AUTO_DELETE_TIMEOUT, agent.ask(&request)).await {
                        Ok(Ok(DeleteVMReply { error })) => error,
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("timed out waiting for the agent".to_owned()),
                    };
                let send_result = scheduler
                    .tell(AutoDeleteFinished {
                        vmid,
                        agent_id,
                        error,
                    })
                    .send()
                    .await;
                if let Err(error) = send_result {
                    warn!(?error, %vmid, "background delete could not notify scheduler");
                }
            });
        }
    }
}

/// Applies a newer agent update, requiring a full snapshot before accepting deltas.
impl Message<AgentUpdated> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: AgentUpdated, ctx: &mut Context<Self, Self::Reply>) {
        let agent_id = msg.actor_id;
        if self.apply_agent_update(msg) {
            self.spawn_auto_deletes(ctx.actor_ref(), agent_id);
        }
    }
}

/// clears the in-flight marker for a background delete
impl Message<AutoDeleteFinished> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: AutoDeleteFinished, _ctx: &mut Context<Self, Self::Reply>) {
        self.settle_auto_delete(msg.vmid, msg.agent_id, msg.error, Instant::now());
    }
}

/// Removes cache state and placements owned by an unreachable agent.
impl Message<AgentUpdaterStopped> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: AgentUpdaterStopped, _ctx: &mut Context<Self, Self::Reply>) {
        self.actor_kinds.remove(&msg.actor_id);
        self.cleanup_agent_actor(msg.actor_id);
    }
}

/// Performs periodic pending-placement expiry, prunes old tombstones, and
/// refreshes resource accounting.
impl Message<ReconcileVmPlacements> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, _msg: ReconcileVmPlacements, _ctx: &mut Context<Self, Self::Reply>) {
        Self::cleanup_unresolved_vm_cache(&mut self.vm_placements, &mut self.vm_data_cache);
        self.prune_vm_tombstones(Instant::now());
        self.invalidate_pending_resources();

        let unplaced_vms: Vec<_> = self
            .vm_manifests
            .iter()
            .filter_map(|(vmid, manifest)| {
                self.vm_placements
                    .get(vmid)
                    .is_none_or(Vec::is_empty)
                    .then_some((*vmid, manifest.clone()))
            })
            .collect();

        for (vmid, config) in unplaced_vms {
            let request = crate::messages::vm::CreateVM {
                vmid,
                config,
                generation: Some(self.next_stamp()),
            };
            match self.schedule_agent(&request) {
                Ok(agent) => {
                    self.remember_agent_ref(&agent);
                    self.remember_vm_delete_owner(vmid, VmDeleteOwner::agent(agent.id()));
                    self.vm_placements
                        .entry(vmid)
                        .or_default()
                        .push(super::VmPlacement {
                            agent_id: agent.id(),
                            lifecycle: super::VmLifecycle::Pending,
                            created_at: std::time::Instant::now(),
                            last_confirmed_at: None,
                        });
                    self.vm_data_cache
                        .entry(vmid)
                        .or_default()
                        .push(CachedVMActor { actor_ref: None });
                    if let Err(error) = agent.tell(&request).send() {
                        warn!(?error, %vmid, "failed to recreate unplaced VM");
                        self.vm_placements.insert(vmid, Vec::new());
                        self.vm_data_cache.remove(&vmid);
                    } else {
                        // The next iteration must account for this new pending reservation.
                        self.invalidate_pending_resources();
                    }
                }
                Err(error) => trace!(?error, %vmid, "no eligible agent to recreate VM"),
            }
        }
    }
}
