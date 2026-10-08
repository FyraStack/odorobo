use crate::{
    ch_driver::actor::VMActor,
    config::Config,
    manifest::{VmManifest, same_create_intent},
    messages::{
        Ping, Pong,
        agent::{
            AgentStatus, AgentStatusUpdate, GetAgentStatus, MembershipChange,
            STATUS_CHANGE_HISTORY_LIMIT, StatusChangeHistory,
        },
        debug::PanicAgent,
        vm::{
            AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
            GetVMInfo, GetVMInfoReply, MigrateVMReceive, MigrateVMReceiveReply, ShutdownVM,
            ShutdownVMReply,
        },
    },
    networking::actor::NetworkAgentActor,
    types::ObjectMetadata,
    utils::actor_names::{NETWORK, VM, vm_actor_id},
    vsock_cid::VsockCidAllocator,
};
use ahash::AHashMap;
use bytesize::ByteSize;
use kameo::prelude::*;
use stable_eyre::{Report, Result};
use std::{
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use sysinfo::System;
use tracing::{error, info, trace, warn};
use ulid::Ulid;

use kameo::error::PanicError;

pub struct VMCacheData {
    actor_ref: ActorRef<VMActor>,
    config: VmManifest,
    vcpus: u32,
    memory_bytes: u64,
    process_exit_confirmed: Arc<AtomicBool>,
}

#[derive(RemoteActor)]
pub struct AgentActor {
    pub vcpus: u32,
    pub memory: ByteSize,
    used_vcpus: u32,
    used_memory_bytes: u64,
    membership_revision: u64,
    status_history: StatusChangeHistory,
    pub config: Config,
    pub vms: AHashMap<Ulid, VMCacheData>,
    /// VM IDs whose actor teardown failed and must not be recreated until an
    /// explicit delete confirms that any retained CID lease is safe to release.
    blocked_vm_ids: AHashMap<Ulid, Arc<AtomicBool>>,
    // pub network_actor: ActorRef<NetworkAgentActor>,
    pub metadata: ObjectMetadata,
}

fn startup_cleanup_failed(result: Result<(), kameo::error::HookError<&Report>>) -> bool {
    match result {
        Err(kameo::error::HookError::Error(error)) => error
            .downcast_ref::<crate::ch_driver::actor::FailedStartupCleanup>()
            .is_some(),
        _ => false,
    }
}

impl AgentActor {
    fn record_membership_change(&mut self, vmid: Ulid, added: bool) {
        self.membership_revision = self.membership_revision.saturating_add(1);
        self.status_history.push_back(MembershipChange {
            revision: self.membership_revision,
            vmid,
            added,
        });
        while self.status_history.len() > STATUS_CHANGE_HISTORY_LIMIT {
            self.status_history.pop_front();
        }
    }

    fn insert_vm(&mut self, vmid: Ulid, cache: VMCacheData) {
        let vcpus = cache.vcpus;
        let memory_bytes = cache.memory_bytes;
        if let Some(previous) = self.vms.insert(vmid, cache) {
            self.used_vcpus = self.used_vcpus.saturating_sub(previous.vcpus);
            self.used_memory_bytes = self.used_memory_bytes.saturating_sub(previous.memory_bytes);
        } else {
            self.record_membership_change(vmid, true);
        }
        self.used_vcpus = self.used_vcpus.saturating_add(vcpus);
        self.used_memory_bytes = self.used_memory_bytes.saturating_add(memory_bytes);
    }

    fn remove_vm(&mut self, vmid: Ulid) -> Option<VMCacheData> {
        let removed = self.vms.remove(&vmid)?;
        self.record_membership_change(vmid, false);
        self.used_vcpus = self.used_vcpus.saturating_sub(removed.vcpus);
        self.used_memory_bytes = self.used_memory_bytes.saturating_sub(removed.memory_bytes);
        Some(removed)
    }

    async fn lookup_vm_actor(vmid: Ulid) -> Option<ActorRef<VMActor>> {
        ActorRef::<VMActor>::lookup(format!("vm:{vmid}"))
            .await
            .ok()
            .flatten()
    }
}

#[allow(clippy::unused_async_trait_impl)]
impl Actor for AgentActor {
    type Args = Config;
    type Error = Report;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        let peer_id = *actor_ref.id().peer_id().unwrap();

        info!(?peer_id, "Agent Actor started!");

        // spawn networking actor
        let network_actor: ActorRef<NetworkAgentActor> =
            NetworkAgentActor::spawn_link(&actor_ref, args.network.clone()).await;
        network_actor.register(NETWORK).await?;

        let sys = System::new_all();

        Ok(Self {
            vcpus: u32::try_from(sys.cpus().len()).unwrap_or(u32::MAX),
            memory: ByteSize::b(sys.total_memory()),
            config: args,
            vms: AHashMap::new(),
            blocked_vm_ids: AHashMap::new(),
            used_vcpus: 0,
            used_memory_bytes: 0,
            membership_revision: 0,
            status_history: StatusChangeHistory::new(),
            metadata: ObjectMetadata::default(),
        })
    }

    async fn on_panic(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        err: PanicError,
    ) -> Result<ControlFlow<ActorStopReason>> {
        error!(
            ?err,
            "Agent actor panicked; stopping because its state cannot be safely rebuilt here"
        );
        Ok(ControlFlow::Break(ActorStopReason::Panicked(err)))
    }

    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>> {
        warn!("Linked actor {id:?} died with reason {reason:?}");

        let removed: Vec<_> = self
            .vms
            .iter()
            .filter(|(_, vm)| vm.actor_ref.id() == id)
            .map(|(vmid, _)| *vmid)
            .collect();
        for vmid in removed {
            // Link-death is delivered before the child's on_stop completes.
            // Keep the ID occupied until teardown has finished, otherwise a
            // replacement can race with deletion of its runtime files/lease.
            let Some(actor_ref) = self.vms.get(&vmid).map(|vm| vm.actor_ref.clone()) else {
                continue;
            };
            let shutdown_result = actor_ref
                .wait_for_shutdown_with_result(|result| {
                    result.map(|_| ()).map_err(|error| error.to_string())
                })
                .await;
            if let Err(error) = shutdown_result {
                warn!(?vmid, %error, "VM teardown failed; blocking ID reuse until explicit deletion");
                let proof = Arc::clone(&self.vms[&vmid].process_exit_confirmed);
                self.blocked_vm_ids.insert(vmid, proof);
            }
            self.remove_vm(vmid);
        }

        Ok(ControlFlow::Continue(()))
    }
}

#[remote_message]
impl Message<CreateVM> for AgentActor {
    type Reply = CreateVMReply;

    async fn handle(&mut self, msg: CreateVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let vmid = msg.vmid;
        if self.blocked_vm_ids.contains_key(&vmid) {
            return CreateVMReply {
                config: None,
                actor_id: None,
                error: Some("VM ID is blocked because its previous actor teardown failed; delete it before recreating".to_owned()),
            };
        }
        if let Some(existing) = self.vms.get(&vmid) {
            if !same_create_intent(&existing.config, &msg.config) {
                warn!(?vmid, actor_id = ?existing.actor_ref.id(), "Rejecting conflicting create for existing VM");
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                    error: Some("conflicting create request for existing VM ID".to_owned()),
                };
            }
            // A failed teardown may leave a dead actor entry in the cache.
            // Verify that this really is a live VM before treating a retry as
            // idempotent rather than claiming a stopped VM still exists.
            if let Err(error) = existing.actor_ref.ask(GetVMInfo { vmid: Some(vmid) }).await {
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                    error: Some(format!(
                        "VM ID is retained because its previous actor is not running: {error}"
                    )),
                };
            }
            info!(?vmid, actor_id = ?existing.actor_ref.id(), "VM already exists; treating create as idempotent");
            return CreateVMReply {
                config: Some(existing.config.clone()),
                actor_id: Some(existing.actor_ref.id().to_bytes()),
                error: None,
            };
        }

        // Spawn and link at the same time.
        let process_exit_confirmed = Arc::new(AtomicBool::new(true));
        let actor_ref = VMActor::spawn_link(
            ctx.actor_ref(),
            (
                vmid,
                Some(msg.config.clone()),
                Arc::clone(&process_exit_confirmed),
            ),
        )
        .await;

        // Wait for on_start to finish before acknowledging creation. This
        // surfaces conversion failures (including vsock CID collisions) to
        // the scheduler instead of caching a dead VM actor as a success.
        let effective_config = match actor_ref.ask(GetVMInfo { vmid: Some(vmid) }).await {
            Ok(info) => info.config,
            Err(error) => {
                actor_ref.kill();
                let shutdown_result = actor_ref
                    .wait_for_shutdown_with_result(|result| {
                        result.map(|_| ()).map_err(|error| error.to_string())
                    })
                    .await;
                let startup_cleanup_failed = actor_ref
                    .with_startup_result(startup_cleanup_failed)
                    .unwrap_or(false);
                if startup_cleanup_failed
                    || !process_exit_confirmed.load(Ordering::SeqCst)
                    || shutdown_result.is_err()
                        && actor_ref
                            .with_startup_result(|result| result.is_ok())
                            .unwrap_or(false)
                {
                    self.blocked_vm_ids
                        .insert(vmid, Arc::clone(&process_exit_confirmed));
                }
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                    error: Some(format!("VM startup failed: {error}")),
                };
            }
        };

        _ = actor_ref.register(vm_actor_id(vmid)).await;
        _ = actor_ref.register(VM).await;
        self.insert_vm(
            vmid,
            VMCacheData {
                actor_ref: actor_ref.clone(),
                config: effective_config
                    .clone()
                    .unwrap_or_else(|| msg.config.clone()),
                vcpus: msg.config.desired.compute.vcpus,
                memory_bytes: msg.config.desired.compute.memory_bytes,
                process_exit_confirmed: Arc::clone(&process_exit_confirmed),
            },
        );

        info!(?vmid, "VM Spawned successfully");
        CreateVMReply {
            config: effective_config,
            actor_id: Some(actor_ref.id().to_bytes()),
            error: None,
        }
    }
}

#[remote_message]
impl Message<MigrateVMReceive> for AgentActor {
    type Reply = MigrateVMReceiveReply;

    async fn handle(
        &mut self,
        msg: MigrateVMReceive,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vmid = msg.vmid;
        // Never replace a live VM cache entry with a second receiver using the
        // same runtime paths and CID lease.
        if self.vms.contains_key(&vmid) || self.blocked_vm_ids.contains_key(&vmid) {
            return MigrateVMReceiveReply {
                listening_address: String::new(),
                error: Some("VM already exists on this agent".to_owned()),
            };
        }
        let process_exit_confirmed = Arc::new(AtomicBool::new(true));
        let actor_ref = VMActor::spawn_link(
            ctx.actor_ref(),
            (vmid, None, Arc::clone(&process_exit_confirmed)),
        )
        .await;

        _ = actor_ref.register(vm_actor_id(vmid)).await;
        _ = actor_ref.register(VM).await;
        self.insert_vm(
            vmid,
            VMCacheData {
                actor_ref: actor_ref.clone(),
                config: msg.config.clone(),
                vcpus: msg.config.desired.compute.vcpus,
                memory_bytes: msg.config.desired.compute.memory_bytes,
                process_exit_confirmed: Arc::clone(&process_exit_confirmed),
            },
        );

        let reply = match actor_ref.ask(msg).await {
            Ok(reply) => reply,
            Err(error) => MigrateVMReceiveReply {
                listening_address: String::new(),
                error: Some(format!("failed to start migration receiver: {error}")),
            },
        };

        if reply.error.is_some() {
            actor_ref.kill();
            let failed = actor_ref
                .wait_for_shutdown_with_result(|result| result.is_err())
                .await;
            if failed || !process_exit_confirmed.load(Ordering::SeqCst) {
                self.blocked_vm_ids
                    .insert(vmid, Arc::clone(&process_exit_confirmed));
            }
            self.remove_vm(vmid);
        }

        reply
    }
}

#[remote_message]
impl Message<DeleteVM> for AgentActor {
    type Reply = DeleteVMReply;

    async fn handle(
        &mut self,
        msg: DeleteVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let cached_actor = self
            .vms
            .get(&msg.vmid)
            .map(|cache_data| cache_data.actor_ref.clone());
        let actor = match cached_actor {
            Some(actor) => Some(actor),
            None => Self::lookup_vm_actor(msg.vmid).await,
        };

        if let Some(actor) = actor {
            if let Err(error) = actor.ask(msg.clone()).await {
                warn!(vm_id = %msg.vmid, ?error, "failed to stop VM actor for delete");
                return DeleteVMReply {
                    error: Some(format!("failed to stop VM actor: {error}")),
                };
            }
            // Do not acknowledge deletion (or permit recreation) until on_stop
            // has reaped the old process and finished releasing its CID.
            let shutdown_result = actor
                .wait_for_shutdown_with_result(|result| {
                    result.map(|_| ()).map_err(|error| error.to_string())
                })
                .await;
            if let Err(error) = shutdown_result {
                let proof = self.vms.get(&msg.vmid).map_or_else(
                    || Arc::new(AtomicBool::new(false)),
                    |vm| Arc::clone(&vm.process_exit_confirmed),
                );
                self.blocked_vm_ids.insert(msg.vmid, proof);
                self.remove_vm(msg.vmid);
                return DeleteVMReply {
                    error: Some(format!("VM teardown failed: {error}")),
                };
            }
            self.remove_vm(msg.vmid);
            self.blocked_vm_ids.remove(&msg.vmid);
            return DeleteVMReply { error: None };
        }

        // Shutdown intentionally removes the VM actor while retaining its
        // node-local CID assignment. Release it only if the VM actor recorded
        // confirmed process exit before disappearing.
        let exit_unconfirmed = self
            .blocked_vm_ids
            .get(&msg.vmid)
            .is_some_and(|proof| !proof.load(Ordering::SeqCst));
        if self.blocked_vm_ids.contains_key(&msg.vmid) && !exit_unconfirmed {
            if let Err(error) = VsockCidAllocator::from_environment().mark_process_exited(msg.vmid)
            {
                return DeleteVMReply {
                    error: Some(format!(
                        "failed to persist confirmed VM process exit: {error}"
                    )),
                };
            }
            // The old process exited but runtime cleanup failed. Retry that
            // cleanup before acknowledging deletion and allowing path reuse.
            let runtime_dir = crate::ch_driver::VMInstance::runtime_dir_for(&msg.vmid.to_string());
            if let Err(error) = std::fs::remove_dir_all(runtime_dir)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return DeleteVMReply {
                    error: Some(format!("failed to retry VM runtime cleanup: {error}")),
                };
            }
        }
        match VsockCidAllocator::from_environment().release_stopped(msg.vmid) {
            Ok(had_cid_lease) if exit_unconfirmed && !had_cid_lease => DeleteVMReply {
                error: Some("VM teardown failed and no confirmed VMM-exit record exists; refusing to unblock its ID".to_owned()),
            },
            Ok(_) => {
                self.blocked_vm_ids.remove(&msg.vmid);
                DeleteVMReply { error: None }
            }
            Err(error) => {
                warn!(vm_id = %msg.vmid, ?error, "failed to release stopped VM CID");
                DeleteVMReply {
                    error: Some(format!("failed to release stopped VM CID: {error}")),
                }
            }
        }
    }
}

#[remote_message]
impl Message<ShutdownVM> for AgentActor {
    type Reply = Result<ShutdownVMReply, String>;

    async fn handle(
        &mut self,
        msg: ShutdownVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Some(actor_ref) = Self::lookup_vm_actor(msg.vmid).await {
            trace!(?msg, "Telling VM to shut down");
            let res = actor_ref.tell(msg.clone()).await;
            if let Err(err) = res {
                warn!(vm_id = %msg.vmid, ?err, "failed to shutdown VM actor");
            }
        } else {
            warn!(vm_id = %msg.vmid, "VM actor not found for shutdown");
            return Err("VM actor not found for shutdown".to_owned());
        }

        Ok(ShutdownVMReply)
    }
}
// forward GetVMInfo to VM actor
#[remote_message]
impl Message<GetVMInfo> for AgentActor {
    type Reply = ForwardedReply<GetVMInfo, GetVMInfoReply>;

    async fn handle(
        &mut self,
        msg: GetVMInfo,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let Some(vmid) = msg.vmid else {
            warn!("No vmid provided for Agent Actor GetVMInfo forwarding");
            return ForwardedReply::from_ok(GetVMInfoReply {
                vmid: Ulid::nil(),
                config: None,
            });
        };

        let Some(actor_ref) = Self::lookup_vm_actor(vmid).await else {
            warn!(vm_id = %vmid, "VM actor not found for info lookup");
            return ForwardedReply::from_ok(GetVMInfoReply { vmid, config: None });
        };

        ctx.forward(&actor_ref, msg).await
    }
}

#[remote_message]
#[allow(clippy::unused_async_trait_impl)]
impl Message<AgentListVMs> for AgentActor {
    type Reply = AgentListVMsReply;

    async fn handle(
        &mut self,
        _msg: AgentListVMs,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // look up with cache
        let vms = self.vms.keys().copied().collect();
        // let vms_actors: Vec<_> = RemoteActorRef::<VMActor>::lookup_all("vm").collect().await;

        // let mut vms = Vec::new();
        // for actor in vms_actors.into_iter().flatten() {
        //     trace!(?actor, "looking up VM info");
        //     if let Ok(reply) = actor.ask(&GetVMInfo).await {
        //         vms.push(reply.vmid);
        //     }
        // }

        AgentListVMsReply { vms }
    }
}

#[remote_message]
#[allow(clippy::unused_async_trait_impl)]
impl Message<Ping> for AgentActor {
    type Reply = Pong;

    async fn handle(&mut self, _msg: Ping, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        Pong
    }
}

#[remote_message]
impl Message<PanicAgent> for AgentActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: PanicAgent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        tracing::info!("panicking");
        panic!();
    }
}

#[remote_message]
#[allow(clippy::unused_async_trait_impl)]
impl Message<GetAgentStatus> for AgentActor {
    type Reply = AgentStatusUpdate;

    async fn handle(
        &mut self,
        msg: GetAgentStatus,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let used_vcpus = self
            .used_vcpus
            .saturating_add(self.config.get_reserved_vcpus());
        let used_ram = ByteSize::b(self.used_memory_bytes);
        let full_status = || AgentStatus {
            hostname: self.config.get_hostname().to_owned(),
            vcpus: self.vcpus,
            ram: self.memory,
            vms: {
                let mut vms: Vec<_> = self.vms.keys().copied().collect();
                vms.sort_unstable();
                vms
            },
            used_vcpus,
            used_ram,
            metadata: self.metadata.clone(),
        };

        if msg.initial
            || msg.since_revision > self.membership_revision
            || self
                .status_history
                .front()
                .is_some_and(|change| msg.since_revision.saturating_add(1) < change.revision)
        {
            return AgentStatusUpdate::Full {
                revision: self.membership_revision,
                status: full_status(),
            };
        }

        let mut latest_changes = AHashMap::new();
        for change in self
            .status_history
            .iter()
            .filter(|change| change.revision > msg.since_revision)
        {
            latest_changes.insert(change.vmid, change.added);
        }
        let mut added = Vec::with_capacity(latest_changes.len());
        let mut removed = Vec::with_capacity(latest_changes.len());
        for (vmid, added_change) in latest_changes {
            if added_change {
                added.push(vmid);
            } else {
                removed.push(vmid);
            }
        }
        added.sort_unstable();
        removed.sort_unstable();
        AgentStatusUpdate::Delta {
            revision: self.membership_revision,
            added,
            removed,
            used_vcpus,
            used_ram,
        }
    }
}

#[cfg(test)]
mod startup_cleanup_tests {
    use super::startup_cleanup_failed;
    use crate::ch_driver::actor::FailedStartupCleanup;
    use kameo::error::HookError;
    use stable_eyre::Report;

    #[test]
    fn failed_console_cleanup_preserves_quarantine_even_when_startup_failed() {
        let cleanup_error = Report::new(FailedStartupCleanup(
            "cleanup after confirmed reap failed".to_owned(),
        ));
        assert!(startup_cleanup_failed(Err(HookError::Error(
            &cleanup_error
        ))));
        let validation_error = Report::msg("invalid manifest before VMM spawn");
        assert!(!startup_cleanup_failed(Err(HookError::Error(
            &validation_error
        ))));
        assert!(!startup_cleanup_failed(Ok(())));
    }
}
