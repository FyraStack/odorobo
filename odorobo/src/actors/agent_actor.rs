use crate::{
    ch_driver::{
        actor::{VMActor, VMCleanupProof},
        transform::{ConfigTransform, storage::StorageDriverTransformer},
    },
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
    vsock_cid::{ActiveVsockLease, VsockCidAllocator},
};
use ahash::AHashMap;
use bytesize::ByteSize;
use kameo::prelude::*;
use stable_eyre::{Report, Result};
use std::{ops::ControlFlow, sync::Arc, time::Duration};
use sysinfo::System;
use tracing::{error, info, trace, warn};
use ulid::Ulid;

use kameo::error::PanicError;

/// how long the agent waits for its VMs to shut down and reap before exiting
const SHUTDOWN_VM_TIMEOUT: Duration = Duration::from_secs(60);
/// how long a delete waits for a leftover VMM to die after SIGKILL
const LEFTOVER_VMM_KILL_TIMEOUT: Duration = Duration::from_secs(10);

pub struct VMCacheData {
    actor_ref: ActorRef<VMActor>,
    config: VmManifest,
    vcpus: u32,
    memory_bytes: u64,
    cleanup_proof: Arc<VMCleanupProof>,
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
    /// VM IDs whose actor teardown failed. we don't let these get recreated until
    /// an explicit delete confirms any leftover CID lease is safe to release
    blocked_vm_ids: AHashMap<Ulid, Arc<VMCleanupProof>>,
    // pub network_actor: ActorRef<NetworkAgentActor>,
    pub metadata: ObjectMetadata,
}

fn startup_cleanup_failed(result: &Result<(), kameo::error::HookError<&Report>>) -> bool {
    match result {
        Err(kameo::error::HookError::Error(error)) => error
            .downcast_ref::<crate::ch_driver::actor::FailedStartupCleanup>()
            .is_some(),
        _ => false,
    }
}

/// whether a VM actor that already stopped has to block its ID. a startup that
/// failed cleanly with a confirmed exit leaves nothing behind, so it doesn't
fn needs_quarantine(
    actor_ref: &ActorRef<VMActor>,
    shutdown_failed: bool,
    proof: &VMCleanupProof,
) -> bool {
    let startup_cleanup_failed = actor_ref
        .with_startup_result(|result| startup_cleanup_failed(&result))
        .unwrap_or(false);
    let started = actor_ref
        .with_startup_result(|result| result.is_ok())
        .unwrap_or(false);
    startup_cleanup_failed || !proof.process_exit_confirmed() || shutdown_failed && started
}

/// make sure a VMM we lost track of is really gone, killing it if it's still
/// running. returns whether we found one and confirmed its exit
async fn stop_leftover_vmm(
    allocator: &VsockCidAllocator,
    vmid: Ulid,
    proof: Option<&VMCleanupProof>,
) -> Result<bool> {
    let process = match proof.and_then(VMCleanupProof::process) {
        Some(process) => Some(process),
        None => allocator.recorded_process(vmid)?,
    };
    let Some(process) = process else {
        return Ok(false);
    };
    warn!(%vmid, pid = process.pid, "stopping leftover VMM before cleaning up after it");
    process.terminate(LEFTOVER_VMM_KILL_TIMEOUT).await?;
    if let Some(proof) = proof {
        proof.set_process_exit_confirmed(true);
    }
    // a lease we don't hold a token for (like after an agent restart) gets
    // marked stopped through the VMM recorded next to it
    allocator.reconcile_exited(vmid)?;
    Ok(true)
}

/// a VM without vsock exiting doesn't tell us anything about a lease someone
/// else grabbed. we only release it once there's a stopped record on disk
fn persist_cleanup_exit(
    allocator: &VsockCidAllocator,
    vmid: Ulid,
    proof: &VMCleanupProof,
) -> Result<()> {
    if !proof.process_exit_confirmed() {
        return Err(Report::msg("VM process exit was not confirmed"));
    }
    proof.lease_token().map_or_else(
        || allocator.ensure_no_active_lease(vmid),
        |token| allocator.mark_process_exited_owned(vmid, token),
    )
}

fn release_cleanup_lease(
    allocator: &VsockCidAllocator,
    vmid: Ulid,
    proof: Option<&VMCleanupProof>,
) -> Result<bool> {
    if let Some(token) = proof.and_then(VMCleanupProof::lease_token) {
        if !proof.is_some_and(VMCleanupProof::process_exit_confirmed) {
            return Err(Report::msg(
                "VM process exit was not confirmed for the owned vsock lease",
            ));
        }
        allocator.release_owned(vmid, token).map(|()| true)
    } else {
        allocator.release_stopped(vmid)
    }
}

impl AgentActor {
    fn verify_no_active_lease(&mut self, vmid: Ulid) -> Result<()> {
        let allocator = VsockCidAllocator::from_environment();
        // a lease left over from before an agent restart is fine once its VMM is gone
        if let Err(error) = allocator.reconcile_exited(vmid) {
            warn!(%vmid, ?error, "failed to check the VMM behind an old vsock lease");
        }
        let result = allocator.ensure_no_active_lease(vmid);
        if result
            .as_ref()
            .is_err_and(|error| error.downcast_ref::<ActiveVsockLease>().is_some())
        {
            // not being in the cache after a restart doesn't mean an older VMM exited
            self.blocked_vm_ids
                .insert(vmid, Arc::new(VMCleanupProof::new(false)));
        }
        result
    }

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

    /// block a VM ID after its teardown failed. if we can't confirm the VMM
    /// exited we keep it listed (and its resources counted), so the scheduler
    /// doesn't fail it over while it might still be running
    fn quarantine(&mut self, vmid: Ulid, proof: Arc<VMCleanupProof>) {
        let exit_confirmed = proof.process_exit_confirmed();
        self.blocked_vm_ids.insert(vmid, proof);
        if exit_confirmed {
            self.remove_vm(vmid);
        }
    }

    /// cleanup for a VM whose actor is already gone, like after a failed
    /// teardown, a shutdown, or an agent restart
    async fn retry_detached_cleanup(&mut self, vmid: Ulid) -> DeleteVMReply {
        let proof = self.blocked_vm_ids.get(&vmid).cloned();
        let allocator = VsockCidAllocator::from_environment();
        let mut exit_confirmed = proof
            .as_ref()
            .is_some_and(|proof| proof.process_exit_confirmed());
        if !exit_confirmed {
            match stop_leftover_vmm(&allocator, vmid, proof.as_deref()).await {
                Ok(stopped) => exit_confirmed = stopped,
                Err(error) => {
                    return DeleteVMReply {
                        error: Some(format!("failed to stop leftover VMM: {error}")),
                    };
                }
            }
        }
        if exit_confirmed {
            if let Some(proof) = &proof
                && let Err(error) = persist_cleanup_exit(&allocator, vmid, proof)
            {
                return DeleteVMReply {
                    error: Some(format!(
                        "failed to persist confirmed VM process exit: {error}"
                    )),
                };
            }
            // leftover storage leases are process-global, so we can still retry
            // after the failed VM actor/transformer is gone. don't detach
            // anything until we know for sure the VMM exited
            if let Err(error) = StorageDriverTransformer::default().teardown(
                &vmid.to_string(),
                &mut cloud_hypervisor_client::models::VmConfig::default(),
            ) {
                return DeleteVMReply {
                    error: Some(format!("failed to retry VM storage cleanup: {error}")),
                };
            }
            // the old process exited but runtime cleanup failed. retry it before
            // we ack the delete and let the paths get reused
            let runtime_dir = crate::ch_driver::VMInstance::runtime_dir_for(&vmid.to_string());
            if let Err(error) = std::fs::remove_dir_all(runtime_dir)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return DeleteVMReply {
                    error: Some(format!("failed to retry VM runtime cleanup: {error}")),
                };
            }
        }
        // don't fall back to release_stopped when we have a token. someone else
        // might've grabbed a new lease since we marked it (or since an earlier
        // delete failed), so removal has to check this exact token under the lock
        match release_cleanup_lease(&allocator, vmid, proof.as_deref()) {
            Ok(had_cid_lease) if proof.is_some() && !exit_confirmed && !had_cid_lease => DeleteVMReply {
                error: Some("VM teardown failed and its VMM exit can't be confirmed; refusing to unblock its ID".to_owned()),
            },
            Ok(_) => {
                self.blocked_vm_ids.remove(&vmid);
                self.remove_vm(vmid);
                DeleteVMReply { error: None }
            }
            Err(error) => {
                warn!(vm_id = %vmid, ?error, "failed to release stopped VM CID");
                DeleteVMReply {
                    error: Some(format!("failed to release stopped VM CID: {error}")),
                }
            }
        }
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

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<()> {
        // stop the VMs ourselves so each one reaps its VMM and marks its lease
        // stopped. otherwise a restart leaves every lease looking active
        let actors: Vec<_> = self.vms.values().map(|vm| vm.actor_ref.clone()).collect();
        if actors.is_empty() {
            return Ok(());
        }
        info!(
            count = actors.len(),
            ?reason,
            "stopping VMs before the agent exits"
        );
        let mut stops = tokio::task::JoinSet::new();
        for actor in actors {
            stops.spawn(async move {
                if actor.is_alive()
                    && let Err(error) = actor.stop_gracefully().await
                {
                    warn!(?error, "failed to ask VM actor to stop");
                }
                actor.wait_for_shutdown().await;
            });
        }
        let all_stopped = async { while stops.join_next().await.is_some() {} };
        if tokio::time::timeout(SHUTDOWN_VM_TIMEOUT, all_stopped)
            .await
            .is_err()
        {
            warn!("timed out stopping VMs, their vsock leases stay active until they're deleted");
        }
        Ok(())
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
            // link death shows up before the child's on_stop finishes.
            // keep the ID taken until teardown is done, or a new VM could race us
            // while we're still deleting its runtime files and CID lease
            let Some(actor_ref) = self.vms.get(&vmid).map(|vm| vm.actor_ref.clone()) else {
                continue;
            };
            let shutdown_result = actor_ref
                .wait_for_shutdown_with_result(|result| {
                    result.map(|_| ()).map_err(|error| error.to_string())
                })
                .await;
            let proof = Arc::clone(&self.vms[&vmid].cleanup_proof);
            if let Err(error) = shutdown_result {
                warn!(?vmid, %error, "VM teardown failed; blocking ID reuse until explicit deletion");
                self.quarantine(vmid, proof);
            } else if !proof.process_exit_confirmed() {
                warn!(
                    ?vmid,
                    "VM stopped without a confirmed exit; blocking ID reuse until explicit deletion"
                );
                self.quarantine(vmid, proof);
            } else {
                self.remove_vm(vmid);
            }
        }

        Ok(ControlFlow::Continue(()))
    }
}

#[remote_message]
impl Message<CreateVM> for AgentActor {
    type Reply = CreateVMReply;

    #[allow(
        clippy::too_many_lines,
        reason = "keeps creation, cleanup proof and cache updates ordered"
    )]
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
            // a failed teardown can leave a dead actor in the cache. make sure
            // the VM is actually alive before treating the retry as idempotent,
            // so we don't claim a stopped VM still exists
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

        if let Err(error) = self.verify_no_active_lease(vmid) {
            return CreateVMReply {
                config: None,
                actor_id: None,
                error: Some(error.to_string()),
            };
        }

        // spawn and link at the same time. hold onto the exit and lease info
        // even if startup never gives us a usable actor
        let cleanup_proof = Arc::new(VMCleanupProof::new(true));
        let actor_ref = VMActor::spawn_link(
            ctx.actor_ref(),
            (vmid, Some(msg.config.clone()), Arc::clone(&cleanup_proof)),
        )
        .await;

        // wait for on_start to finish before we say the VM was created. that way
        // conversion failures (like vsock CID collisions) go back to the
        // scheduler instead of us caching a dead VM actor as a success
        let effective_config = match actor_ref.ask(GetVMInfo { vmid: Some(vmid) }).await {
            Ok(info) => info.config,
            Err(error) => {
                actor_ref.kill();
                let shutdown_result = actor_ref
                    .wait_for_shutdown_with_result(|result| {
                        result.map(|_| ()).map_err(|error| error.to_string())
                    })
                    .await;
                if needs_quarantine(&actor_ref, shutdown_result.is_err(), &cleanup_proof) {
                    self.blocked_vm_ids.insert(vmid, Arc::clone(&cleanup_proof));
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
                cleanup_proof: Arc::clone(&cleanup_proof),
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
        // don't ever replace a live VM in the cache with a second receiver that
        // would use the same runtime paths and CID lease
        if self.vms.contains_key(&vmid) || self.blocked_vm_ids.contains_key(&vmid) {
            return MigrateVMReceiveReply {
                listening_address: String::new(),
                error: Some("VM already exists on this agent".to_owned()),
            };
        }
        if let Err(error) = self.verify_no_active_lease(vmid) {
            return MigrateVMReceiveReply {
                listening_address: String::new(),
                error: Some(error.to_string()),
            };
        }
        let cleanup_proof = Arc::new(VMCleanupProof::new(true));
        let actor_ref =
            VMActor::spawn_link(ctx.actor_ref(), (vmid, None, Arc::clone(&cleanup_proof))).await;

        _ = actor_ref.register(vm_actor_id(vmid)).await;
        _ = actor_ref.register(VM).await;
        self.insert_vm(
            vmid,
            VMCacheData {
                actor_ref: actor_ref.clone(),
                config: msg.config.clone(),
                vcpus: msg.config.desired.compute.vcpus,
                memory_bytes: msg.config.desired.compute.memory_bytes,
                cleanup_proof: Arc::clone(&cleanup_proof),
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
            if needs_quarantine(&actor_ref, failed, &cleanup_proof) {
                self.quarantine(vmid, Arc::clone(&cleanup_proof));
            } else {
                self.remove_vm(vmid);
            }
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
        let vmid = msg.vmid;
        // a blocked VM's actor is already gone, even if we kept it listed
        // because its exit couldn't be confirmed
        if !self.blocked_vm_ids.contains_key(&vmid) {
            let cached_actor = self
                .vms
                .get(&vmid)
                .map(|cache_data| cache_data.actor_ref.clone());
            let actor = match cached_actor {
                Some(actor) => Some(actor),
                None => Self::lookup_vm_actor(vmid).await,
            };

            if let Some(actor) = actor {
                if let Err(error) = actor.ask(msg.clone()).await {
                    warn!(vm_id = %vmid, ?error, "failed to stop VM actor for delete");
                    return DeleteVMReply {
                        error: Some(format!("failed to stop VM actor: {error}")),
                    };
                }
                // don't ack the delete (or let it get recreated) until on_stop has
                // reaped the old process and finished releasing its CID
                let shutdown_result = actor
                    .wait_for_shutdown_with_result(|result| {
                        result.map(|_| ()).map_err(|error| error.to_string())
                    })
                    .await;
                let proof = self.vms.get(&vmid).map(|vm| Arc::clone(&vm.cleanup_proof));
                let error = match (&shutdown_result, &proof) {
                    (Err(error), _) => Some(format!("VM teardown failed: {error}")),
                    (Ok(()), Some(proof)) if !proof.process_exit_confirmed() => {
                        Some("VM stopped but its VMM exit wasn't confirmed".to_owned())
                    }
                    (Ok(()), _) => None,
                };
                if let Some(error) = error {
                    let proof = proof.unwrap_or_else(|| Arc::new(VMCleanupProof::new(false)));
                    self.quarantine(vmid, proof);
                    return DeleteVMReply { error: Some(error) };
                }
                self.remove_vm(vmid);
                self.blocked_vm_ids.remove(&vmid);
                return DeleteVMReply { error: None };
            }
        }

        self.retry_detached_cleanup(vmid).await
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
    use super::{
        persist_cleanup_exit, release_cleanup_lease, startup_cleanup_failed, stop_leftover_vmm,
    };
    use crate::ch_driver::actor::FailedStartupCleanup;
    use kameo::error::HookError;
    use stable_eyre::Report;

    #[test]
    fn no_vsock_blocked_retry_cannot_mark_or_delete_newly_active_lease() {
        use crate::{ch_driver::actor::VMCleanupProof, vsock_cid::VsockCidAllocator};
        let directory =
            std::env::temp_dir().join(format!("odorobo-no-vsock-retry-{}", ulid::Ulid::generate()));
        let allocator = VsockCidAllocator::new(directory.join("registry.json"));
        let vmid = ulid::Ulid::generate();
        // this is the shared `VMCleanupProof` a blocked no-vsock actor keeps when
        // the process was reaped but storage/runtime cleanup failed
        let proof = std::sync::Arc::new(VMCleanupProof::new(true));
        allocator.ensure_no_active_lease(vmid).unwrap();
        let reservation = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        drop(persist_cleanup_exit(&allocator, vmid, &proof).unwrap_err());
        drop(release_cleanup_lease(&allocator, vmid, Some(&proof)).unwrap_err());
        drop(allocator.ensure_no_active_lease(vmid).unwrap_err());
        // it's only safe once the real lease owner writes down that it exited
        allocator
            .mark_process_exited_owned(vmid, reservation.token)
            .unwrap();
        persist_cleanup_exit(&allocator, vmid, &proof).unwrap();
        assert!(release_cleanup_lease(&allocator, vmid, Some(&proof)).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn blocked_owned_retry_rechecks_generation_between_exit_mark_and_release() {
        use crate::{ch_driver::actor::VMCleanupProof, vsock_cid::VsockCidAllocator};
        let directory =
            std::env::temp_dir().join(format!("odorobo-owned-retry-{}", ulid::Ulid::generate()));
        let allocator = VsockCidAllocator::new(directory.join("registry.json"));
        let vmid = ulid::Ulid::generate();
        let initial = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        let proof = VMCleanupProof::new(true);
        proof.own_lease(initial.token);
        persist_cleanup_exit(&allocator, vmid, &proof).unwrap();
        // someone else grabs the same CID while we're cleaning up or retrying
        let newer = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        drop(release_cleanup_lease(&allocator, vmid, Some(&proof)).unwrap_err());
        drop(persist_cleanup_exit(&allocator, vmid, &proof).unwrap_err());
        drop(allocator.ensure_no_active_lease(vmid).unwrap_err());
        // even after the newer VMM exits, an old retry can't delete its lease by
        // falling back to a stopped record from a different generation
        allocator
            .mark_process_exited_owned(vmid, newer.token)
            .unwrap();
        drop(release_cleanup_lease(&allocator, vmid, Some(&proof)).unwrap_err());
        allocator.release_owned(vmid, newer.token).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_console_cleanup_preserves_quarantine_even_when_startup_failed() {
        let cleanup_error = Report::new(FailedStartupCleanup(
            "cleanup after confirmed reap failed".to_owned(),
        ));
        assert!(startup_cleanup_failed(&Err(HookError::Error(
            &cleanup_error
        ))));
        let wrapped_cleanup_error = cleanup_error.wrap_err("original startup error");
        assert!(startup_cleanup_failed(&Err(HookError::Error(
            &wrapped_cleanup_error
        ))));
        let validation_error = Report::msg("invalid manifest before VMM spawn");
        assert!(!startup_cleanup_failed(&Err(HookError::Error(
            &validation_error
        ))));
        assert!(!startup_cleanup_failed(&Ok(())));
    }

    #[tokio::test]
    async fn delete_after_restart_kills_the_orphaned_vmm_and_frees_its_lease() {
        use crate::{vmm_process::ProcessIdentity, vsock_cid::VsockCidAllocator};
        let directory =
            std::env::temp_dir().join(format!("odorobo-orphan-vmm-{}", ulid::Ulid::generate()));
        let allocator = VsockCidAllocator::new(directory.join("registry.json"));
        let vmid = ulid::Ulid::generate();
        let reservation = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        let mut vmm = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let process = ProcessIdentity::of(vmm.id().unwrap()).unwrap();
        allocator
            .record_process_owned(vmid, reservation.token, process)
            .unwrap();

        // no proof, like an agent that restarted while the VMM kept running
        assert!(stop_leftover_vmm(&allocator, vmid, None).await.unwrap());
        assert!(!process.is_running().unwrap());
        assert!(release_cleanup_lease(&allocator, vmid, None).unwrap());
        drop(vmm.wait().await);

        // nothing recorded means nothing to kill
        assert!(!stop_leftover_vmm(&allocator, vmid, None).await.unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
