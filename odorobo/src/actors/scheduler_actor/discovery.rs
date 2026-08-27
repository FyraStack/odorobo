//! Remote actor discovery, polling tasks, and their internal messages.

use std::time::Duration;

use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use stable_eyre::{Report, eyre::eyre};
use tracing::{trace, warn};

use crate::actors::agent_actor::AgentActor;
use crate::ch_driver::actor::VMActor;
use crate::messages::agent::{AgentStatusUpdate, GetAgentStatus, apply_status_update};
use crate::messages::vm::{GetVMHeartbeat, GetVMInfo};
use crate::utils::actor_names::{AGENT, VM};

use super::{
    AgentActorDiscovered, AgentUpdated, AgentUpdaterStopped, CachedActorKind, CachedAgentActor,
    CachedVMActor, ReconcileVmPlacements, SchedulerActor, VmActorDiscovered, VmUpdated,
    VmUpdaterStopped,
};

impl SchedulerActor {
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

impl Message<VmUpdaterStopped> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: VmUpdaterStopped, _ctx: &mut Context<Self, Self::Reply>) {
        self.vm_keepalive_tasks.remove(&msg.actor_id);
        self.actor_kinds.remove(&msg.actor_id);
        let vmid = self.vm_actorid_ulid_map.remove(&msg.actor_id);
        Self::remove_vm_actor(msg.actor_id, &mut self.vm_data_cache);

        if let Some(vmid) = vmid
            && self
                .vm_data_cache
                .get(&vmid)
                .is_none_or(|entries| entries.iter().all(|entry| entry.actor_ref.is_none()))
        {
            Self::remove_vm_state(
                vmid,
                &mut self.vm_manifests,
                &mut self.vm_placements,
                &mut self.vm_data_cache,
            );
        }
    }
}

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
    }
}

impl Message<AgentUpdaterStopped> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, msg: AgentUpdaterStopped, _ctx: &mut Context<Self, Self::Reply>) {
        self.agent_keepalive_tasks.remove(&msg.actor_id);
        self.actor_kinds.remove(&msg.actor_id);
        self.agent_data_cache.remove(&msg.actor_id);
        self.agent_vm_index.remove(&msg.actor_id);
        Self::remove_agent_placements(
            msg.actor_id,
            &mut self.vm_manifests,
            &mut self.vm_placements,
            &mut self.vm_data_cache,
        );
        self.invalidate_pending_resources();
    }
}

impl Message<ReconcileVmPlacements> for SchedulerActor {
    type Reply = ();

    async fn handle(&mut self, _msg: ReconcileVmPlacements, _ctx: &mut Context<Self, Self::Reply>) {
        Self::cleanup_unresolved_vm_cache(
            &mut self.vm_manifests,
            &mut self.vm_placements,
            &mut self.vm_data_cache,
        );
        self.invalidate_pending_resources();
    }
}
