//! Actor lifecycle implementation and public scheduler message handlers.

use std::ops::ControlFlow;
use std::time::Instant;

use ahash::AHashMap;
use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use stable_eyre::{Report, eyre::eyre};
use tracing::{info, warn};

use crate::actors::agent_actor::AgentActor;
use crate::ch_driver::actor::VMActor;
use crate::manifest::same_create_intent;
use crate::messages::vm::{
    AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
    GetConsoleHistory, GetConsoleHistoryReply, GetVMInfo, GetVMInfoReply, SendConsoleInput,
    SendConsoleInputReply, ShutdownVM, ShutdownVMReply,
};
use crate::messages::{Ping, Pong};
use crate::utils::actor_names::{AGENT, vm_actor_id};

use super::deletion::{DeleteAttempt, delete_owner_outcome, plan_delete_settlement};
use super::{
    CachedActorKind, CachedVMActor, SchedulerActor, VmDeleteOwner, VmLifecycle, VmPlacement,
    VmTombstoneState,
};

/// Owns scheduler initialization and cleanup for linked remote actors.
///
/// Losing an agent removes its placements. Losing a VM removes only its actor
/// cache entry unless no discovered actor or placeholder remains for that VM.
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
            vm_manifests: AHashMap::new(),
            vm_effective_manifests: AHashMap::new(),
            vm_tombstones: AHashMap::new(),
            vm_auto_deletes: AHashMap::new(),
            vm_leftover_owners: AHashMap::new(),
            known_agent_refs: AHashMap::new(),
            vm_delete_targets: AHashMap::new(),
            vm_placements: AHashMap::new(),
            vm_data_cache: AHashMap::new(),
            vm_keepalive_tasks: AHashMap::new(),
            pending_resources_cache: None,
            agent_vm_index: AHashMap::new(),
            actor_kinds: AHashMap::new(),
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

        match self.actor_kinds.remove(&id) {
            Some(CachedActorKind::Agent) => self.cleanup_agent_actor(id),
            Some(CachedActorKind::Vm) => self.cleanup_vm_actor(id),
            None => {}
        }

        Ok(ControlFlow::Continue(()))
    }
}
/// Optimistically reserves a placement, then forwards VM creation to the chosen agent.
///
/// A successful reply confirms agent acceptance, not observed VM execution. A failed
/// request is rolled back only when discovery cannot find a VM actor, preserving state
/// for eventual reconciliation when the request result was lost or delayed.
impl Message<CreateVM> for SchedulerActor {
    type Reply = Result<CreateVMReply, Report>;

    #[allow(
        clippy::too_many_lines,
        reason = "keeps correlated create caches and uncertainty handling together"
    )]
    async fn handle(
        &mut self,
        msg: CreateVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.check_create_allowed(msg.vmid)
            .map_err(|error| eyre!(error))?;
        if let Some(existing) = self.vm_manifests.get(&msg.vmid) {
            let matches_intent = same_create_intent(existing, &msg.config)
                || self
                    .vm_effective_manifests
                    .get(&msg.vmid)
                    .is_some_and(|effective| same_create_intent(effective, &msg.config));
            if !matches_intent {
                return Err(eyre!("conflicting create request for existing VM ID"));
            }

            if !self.create_is_confirmed(msg.vmid) {
                return Err(eyre!(
                    "create outcome is unknown; VM creation is pending agent acceptance or discovery"
                ));
            }

            let actor_id = self
                .vm_actorid_ulid_map
                .iter()
                .find_map(|(actor_id, vmid)| (*vmid == msg.vmid).then(|| actor_id.to_bytes()));
            return Ok(CreateVMReply {
                config: Some(
                    self.vm_effective_manifests
                        .get(&msg.vmid)
                        .unwrap_or(existing)
                        .clone(),
                ),
                actor_id,
                error: None,
            });
        }

        let target_agent = self.schedule_agent(&msg)?;

        self.remember_agent_ref(&target_agent);
        let delete_owner = VmDeleteOwner::agent(target_agent.id());
        let delete_owner_was_new = !self
            .vm_delete_targets
            .get(&msg.vmid)
            .is_some_and(|targets| targets.contains_key(&delete_owner));
        self.remember_vm_delete_owner(msg.vmid, delete_owner);
        self.vm_tombstones.remove(&msg.vmid);
        self.vm_manifests.insert(msg.vmid, msg.config.clone());
        self.invalidate_pending_resources();
        self.vm_placements
            .entry(msg.vmid)
            .or_default()
            .push(VmPlacement {
                agent_id: target_agent.id(),
                lifecycle: VmLifecycle::Pending,
                created_at: Instant::now(),
                last_confirmed_at: None,
            });
        self.vm_data_cache
            .entry(msg.vmid)
            .or_default()
            .push(CachedVMActor { actor_ref: None });

        let mut startup_outcome_unknown = false;
        // keep the actor ID from a rejection too, so rollback can drop its mapping
        let mut rejected_actor_id = None;
        let reply: Result<CreateVMReply, Report> = match target_agent.ask(&msg).await {
            Ok(CreateVMReply {
                config,
                actor_id,
                error: None,
            }) => Ok(CreateVMReply {
                config,
                actor_id,
                error: None,
            }),
            Ok(CreateVMReply {
                actor_id,
                error: Some(error),
                ..
            }) => {
                rejected_actor_id = actor_id
                    .as_deref()
                    .and_then(|bytes| ActorId::from_bytes(bytes).ok());
                Err(eyre!("agent failed to create VM: {error}"))
            }
            Err(error) => {
                startup_outcome_unknown = true;
                Err(eyre!(error.to_string()))
            }
        };

        if let Ok(reply) = &reply
            && let Some(actor_id_bytes) = &reply.actor_id
            && let Ok(actor_id) = ActorId::from_bytes(actor_id_bytes)
        {
            self.remember_vm_delete_owner(msg.vmid, VmDeleteOwner::vm(actor_id));
            self.vm_actorid_ulid_map.insert(actor_id, msg.vmid);
        }

        // a transport or discovery failure doesn't mean startup actually failed.
        // only roll back if the agent explicitly rejected it and a lookup worked
        // and found no VM actor
        if reply.is_err()
            && !startup_outcome_unknown
            && matches!(
                RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await,
                Ok(None)
            )
        {
            Self::rollback_failed_create(
                msg.vmid,
                false,
                rejected_actor_id,
                &mut self.vm_actorid_ulid_map,
                &mut self.vm_manifests,
                &mut self.vm_effective_manifests,
                &mut self.vm_placements,
                &mut self.vm_data_cache,
            );

            let cleanup_confirmed = matches!(
                target_agent
                    .ask(&DeleteVM {
                        vmid: msg.vmid,
                        force: false,
                    })
                    .await,
                Ok(DeleteVMReply { error: None })
            );
            self.settle_failed_create_cleanup(
                msg.vmid,
                delete_owner,
                delete_owner_was_new,
                cleanup_confirmed,
            );
        }

        if let Ok(reply) = &reply
            && let Some(effective_config) = &reply.config
        {
            self.vm_effective_manifests
                .insert(msg.vmid, effective_config.clone());
        }

        reply
    }
}

impl Message<GetConsoleHistory> for SchedulerActor {
    type Reply = Result<GetConsoleHistoryReply, Report>;

    async fn handle(
        &mut self,
        msg: GetConsoleHistory,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?;
        tracing::trace!(?vm, vmid = %msg.vmid, "GetConsoleHistory");
        if let Some(vm) = vm {
            Ok(vm.ask(&msg).await?)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

impl Message<SendConsoleInput> for SchedulerActor {
    type Reply = Result<SendConsoleInputReply, Report>;

    async fn handle(
        &mut self,
        msg: SendConsoleInput,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(msg.vmid)).await?;
        tracing::trace!(
            ?vm,
            vmid = %msg.vmid,
            bytes = msg.input.len(),
            "SendConsoleInput"
        );
        if let Some(vm) = vm {
            Ok(vm.ask(&msg).await?)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

/// stops recreation right away, but keeps track of cleanup owners (evicted ones
/// too) until the agents confirm the process exited and the node-local CID was released.
/// with `force`, owners we can't reach at all get dropped instead of failing the
/// delete. an agent that answers with an error still fails it
impl Message<DeleteVM> for SchedulerActor {
    type Reply = Result<DeleteVMReply, Report>;

    async fn handle(
        &mut self,
        msg: DeleteVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let already_deleted = self.vm_tombstone_state(msg.vmid) == Some(VmTombstoneState::Deleted)
            && !self.vm_delete_targets.contains_key(&msg.vmid);
        self.remove_vm_intent(msg.vmid);
        self.set_vm_tombstone(msg.vmid, VmTombstoneState::DeletePending);
        // the unique VM name can show us a migration owner before its agent status
        // shows up. don't ask the VM actors directly though, since they can't ack
        // the agent's stopped lease cleanup
        let mut vms = RemoteActorRef::<VMActor>::lookup_all(vm_actor_id(msg.vmid));
        while let Some(vm) = vms.try_next().await? {
            self.remember_vm_delete_owner(msg.vmid, VmDeleteOwner::vm(vm.id()));
        }
        if already_deleted && !self.vm_delete_targets.contains_key(&msg.vmid) {
            self.set_vm_tombstone(msg.vmid, VmTombstoneState::Deleted);
            return Ok(DeleteVMReply { error: None });
        }
        // we still need discovery even with cached targets. an agent that restarted
        // on the same peer has a new actor ID, and that has to replace the old route
        let mut discovered = RemoteActorRef::<AgentActor>::lookup_all(AGENT);
        let mut available = AHashMap::new();
        while let Some(agent) = discovered.try_next().await? {
            available.insert(agent.id(), agent.clone());
            self.remember_agent_ref(&agent);
        }
        if self
            .vm_delete_targets
            .get(&msg.vmid)
            .is_none_or(|targets| targets.is_empty())
        {
            // no owner history and no agent has reported holding anything for
            // this VM, so just ask the agents we know about now, not every dead
            // agent ever. agents report `retained_vms` in their status, which
            // rebuilds owner history after a scheduler restart
            available.extend(
                self.agent_data_cache
                    .values()
                    .map(|agent| (agent.actor_ref.id(), agent.actor_ref.clone())),
            );
            for agent in available.values() {
                self.remember_vm_delete_owner(msg.vmid, VmDeleteOwner::agent(agent.id()));
            }
        }
        let targets: Vec<_> = self
            .vm_delete_targets
            .get(&msg.vmid)
            .into_iter()
            .flat_map(|targets| targets.iter())
            .filter(|(_, target)| !target.confirmed)
            .map(|(owner, target)| (*owner, target.actor_ref.clone()))
            .collect();

        let mut outcomes = Vec::with_capacity(targets.len());
        for (owner, retained) in targets {
            // the registry can return both the old and the replacement actor on
            // one peer. don't let a stale route hide a working one forever
            let mut candidates: AHashMap<_, _> = available
                .iter()
                .filter(|(id, _)| owner.matches_agent(**id))
                .map(|(id, agent)| (*id, agent.clone()))
                .collect();
            if let Some(agent) = retained.filter(|agent| owner.matches_agent(agent.id())) {
                candidates.entry(agent.id()).or_insert(agent);
            }
            let mut attempts = Vec::with_capacity(candidates.len());
            for agent in candidates.values() {
                let attempt = match agent.ask(&msg).await {
                    Ok(DeleteVMReply { error: None }) => DeleteAttempt::Acked,
                    Ok(DeleteVMReply { error: Some(error) }) => DeleteAttempt::AgentError(error),
                    Err(error) => DeleteAttempt::Unreachable(error.to_string()),
                };
                let acked = attempt == DeleteAttempt::Acked;
                attempts.push(attempt);
                if acked {
                    break;
                }
            }
            outcomes.push((owner, delete_owner_outcome(attempts)));
        }

        let settlement = plan_delete_settlement(outcomes, msg.force);
        self.apply_delete_settlement(msg.vmid, settlement, msg.force)
            .map(|()| DeleteVMReply { error: None })
            .map_err(|error| eyre!(error))
    }
}

/// Looks up a VM actor, forwards shutdown, and suppresses automatic recreation.
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
            vm.tell(&msg).send()?;
            self.remember_vm_delete_owner(msg.vmid, VmDeleteOwner::vm(vm.id()));
            self.remove_vm_intent(msg.vmid);
            Ok(ShutdownVMReply)
        } else {
            Err(eyre!("VM not found"))
        }
    }
}

/// Looks up a VM actor and forwards an info request.
impl Message<GetVMInfo> for SchedulerActor {
    type Reply = Result<GetVMInfoReply, Report>;

    async fn handle(
        &mut self,
        msg: GetVMInfo,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vmid = msg.vmid.ok_or_else(|| eyre!("VM ID is required"))?;
        let vm = RemoteActorRef::<VMActor>::lookup(vm_actor_id(vmid)).await?;

        let Some(vm) = vm else {
            return Ok(GetVMInfoReply { vmid, config: None });
        };

        Ok(vm.ask(&msg).await?)
    }
}

/// Returns the concatenated VM IDs from cached agent status snapshots.
///
/// This is a potentially stale, non-deduplicated observation rather than an
/// authoritative inventory; it performs neither polling nor direct VM-actor queries.
impl Message<AgentListVMs> for SchedulerActor {
    type Reply = Result<AgentListVMsReply, Report>;

    async fn handle(
        &mut self,
        _msg: AgentListVMs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let total_vms = self
            .agent_data_cache
            .values()
            .map(|agent| agent.data.vms.len())
            .sum();
        let mut vms = Vec::with_capacity(total_vms);

        for agent in self.agent_data_cache.values() {
            vms.extend_from_slice(agent.data.vms.as_slice());
        }

        Ok(AgentListVMsReply { vms })
    }
}

/// Provides scheduler actor liveness only; it does not imply cache freshness or readiness.
impl Message<Ping> for SchedulerActor {
    type Reply = Pong;

    async fn handle(&mut self, _msg: Ping, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        Pong
    }
}

#[cfg(test)]
mod create_intent_tests {
    use crate::manifest::{VmManifest, same_create_intent};

    fn vsock_manifest() -> VmManifest {
        serde_json::from_str(include_str!(
            "../../../../docs/fixtures/manifest/vsock.json"
        ))
        .expect("vsock fixture parses")
    }

    #[test]
    fn automatic_and_explicit_cids_are_different_create_intents() {
        let mut automatic = vsock_manifest();
        automatic.desired.vsock.as_mut().unwrap().guest_cid = None;
        let explicit = vsock_manifest();
        assert!(!same_create_intent(&explicit, &automatic));
        assert!(!same_create_intent(&automatic, &explicit));
    }

    #[test]
    fn automatically_assigned_observed_cid_does_not_change_create_intent() {
        let mut effective = vsock_manifest();
        effective.desired.vsock.as_mut().unwrap().guest_cid = None;
        effective.observed = Some(crate::manifest::ObservedState {
            vsock_guest_cid: Some(42),
            ..Default::default()
        });
        let mut requested = effective.clone();
        requested.observed = None;

        assert!(same_create_intent(&effective, &requested));
    }

    #[test]
    fn requested_cid_or_other_manifest_changes_still_conflict() {
        let existing = vsock_manifest();
        let mut different_cid = vsock_manifest();
        different_cid.desired.vsock.as_mut().unwrap().guest_cid = Some(43);
        assert!(!same_create_intent(&existing, &different_cid));

        let mut different_socket = vsock_manifest();
        different_socket.desired.vsock.as_mut().unwrap().socket = "/run/other.sock".to_owned();
        assert!(!same_create_intent(&existing, &different_socket));
    }
}
