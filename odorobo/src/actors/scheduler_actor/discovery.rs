//! Remote actor discovery, polling tasks, and their internal messages.

use std::time::Duration;

use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use stable_eyre::{Report, eyre::eyre};
use tracing::{trace, warn};

use crate::actors::agent_actor::AgentActor;
use crate::ch_driver::actor::VMActor;
use crate::messages::agent::{AgentStatusUpdate, GetAgentStatus, apply_status_update};
use crate::messages::vm::{CreateVM, GetVMHeartbeat, GetVMInfo};
use crate::utils::actor_names::{AGENT, VM};

use super::{
    AgentActorDiscovered, AgentUpdated, AgentUpdaterStopped, CachedActorKind, CachedAgentActor,
    CachedVMActor, ReconcileVmPlacements, SchedulerActor, VmActorDiscovered, VmUpdated,
    VmUpdaterStopped,
};

impl SchedulerActor {
    async fn reconcile_durable_vms_for_agent(
        &mut self,
        actor_id: ActorId,
        actor_ref: RemoteActorRef<AgentActor>,
        hostname: &str,
        observed: &[ulid::Ulid],
    ) {
        let observed: ahash::AHashSet<_> = observed.iter().copied().collect();
        let pending: Vec<_> = self
            .durable_placements
            .iter()
            .filter(|(vmid, placement)| placement.node == hostname && !observed.contains(vmid))
            .filter_map(|(vmid, _)| {
                let config = self.vm_manifests.get(vmid)?.clone();
                let entries = self.vm_placements.entry(*vmid).or_default();
                if entries.iter().any(|entry| {
                    entry.agent_id == actor_id && entry.lifecycle == super::VmLifecycle::Pending
                }) {
                    return None;
                }
                entries.retain(|entry| entry.agent_id != actor_id);
                entries.push(super::VmPlacement {
                    agent_id: actor_id,
                    lifecycle: super::VmLifecycle::Pending,
                    created_at: std::time::Instant::now(),
                    last_confirmed_at: None,
                });
                self.vm_data_cache
                    .entry(*vmid)
                    .or_default()
                    .push(CachedVMActor { actor_ref: None });
                Some(CreateVM {
                    vmid: *vmid,
                    config,
                })
            })
            .collect();

        if !pending.is_empty() {
            self.invalidate_pending_resources();
        }
        for create in pending {
            match tokio::time::timeout(Duration::from_secs(10), actor_ref.ask(&create)).await {
                Ok(Ok(reply)) => {
                    if let Some(actor_id_bytes) = reply.actor_id
                        && let Ok(vm_actor_id) = ActorId::from_bytes(&actor_id_bytes)
                    {
                        self.vm_actorid_ulid_map.insert(vm_actor_id, create.vmid);
                    }
                }
                Ok(Err(error)) => warn!(
                    ?error,
                    vm_id = %create.vmid,
                    %hostname,
                    "Unable to reconcile durable VM placement"
                ),
                Err(_) => warn!(
                    vm_id = %create.vmid,
                    %hostname,
                    "Timed out reconciling durable VM placement"
                ),
            }
        }
    }

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
            if initialized {
                match actor_ref.ask(&GetVMHeartbeat).await {
                    Ok(reply) if reply.error.is_none() => fails = 0,
                    Ok(reply) => {
                        warn!(?reply.error, ?actor_ref, "VM heartbeat reported a VMM failure");
                        fails = fails.saturating_add(1);
                    }
                    Err(_) => fails = fails.saturating_add(1),
                }
            } else if let Ok(data) = actor_ref.ask(&GetVMInfo { vmid: None }).await {
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

        loop {
            let Some(agent_actor) = agent_actor_stream.try_next().await? else {
                break;
            };
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
        let actor_id = msg.actor_ref.id();
        self.vm_actorid_ulid_map.insert(actor_id, vmid);
        if let Some(manifest) = msg.data.config {
            self.vm_manifests.insert(vmid, manifest);
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

/// Applies a newer agent update, requiring a full snapshot before accepting deltas.
impl Message<AgentUpdated> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: AgentUpdated, _ctx: &mut Context<Self, Self::Reply>) {
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
                let (actor_ref, hostname, observed) = {
                    let cached = self.agent_data_cache.get(&msg.actor_id).unwrap();
                    (
                        cached.actor_ref.clone(),
                        cached.data.hostname.clone(),
                        cached.data.vms.clone(),
                    )
                };
                self.reconcile_durable_vms_for_agent(
                    msg.actor_id,
                    actor_ref.clone(),
                    &hostname,
                    &observed,
                )
                .await;
                drop(actor_ref);
            }
            return;
        };

        let revision = match &msg.update {
            AgentStatusUpdate::Full { revision, .. }
            | AgentStatusUpdate::Delta { revision, .. } => *revision,
        };
        if revision <= cached.status_revision {
            return;
        }
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

        let (actor_ref, hostname, observed) = {
            let cached = self.agent_data_cache.get(&msg.actor_id).unwrap();
            (
                cached.actor_ref.clone(),
                cached.data.hostname.clone(),
                cached.data.vms.clone(),
            )
        };
        self.reconcile_durable_vms_for_agent(msg.actor_id, actor_ref.clone(), &hostname, &observed)
            .await;
        drop(actor_ref);
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

/// Performs periodic pending-placement expiry and refreshes resource accounting.
impl Message<ReconcileVmPlacements> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, _msg: ReconcileVmPlacements, _ctx: &mut Context<Self, Self::Reply>) {
        Self::cleanup_unresolved_vm_cache(
            &mut self.vm_manifests,
            &mut self.vm_placements,
            &mut self.vm_data_cache,
        );
        self.invalidate_pending_resources();
        let agents: Vec<_> = self
            .agent_data_cache
            .iter()
            .map(|(actor_id, cached)| {
                (
                    *actor_id,
                    cached.actor_ref.clone(),
                    cached.data.hostname.clone(),
                    cached.data.vms.clone(),
                )
            })
            .collect();
        for (actor_id, actor_ref, hostname, observed) in agents {
            self.reconcile_durable_vms_for_agent(actor_id, actor_ref.clone(), &hostname, &observed)
                .await;
            drop(actor_ref);
        }
    }
}
