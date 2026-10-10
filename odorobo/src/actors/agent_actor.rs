use crate::cluster_state::{PlacementLifecycle, PlacementRecord, StateStore};
use crate::{
    ch_driver::actor::VMActor,
    config::Config,
    manifest::VmManifest,
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

const VM_ACTOR_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);
// Leave time for the scheduler's 30-second outer delete request to receive this failure.
const VM_ACTOR_DELETE_TIMEOUT: Duration = Duration::from_secs(25);

pub struct VMCacheData {
    actor_ref: ActorRef<VMActor>,
    config: VmManifest,
    vcpus: u32,
    memory_bytes: u64,
}

/// A stop confirmation is process-local evidence, never inferred from an empty
/// runtime cache. Keep only the latest completed incarnation for each VM ID.
#[derive(Default)]
struct ConfirmedStopFences(AHashMap<Ulid, PlacementRecord>);

impl ConfirmedStopFences {
    fn confirm(&mut self, placement: &PlacementRecord) {
        self.0.insert(placement.vmid, placement.clone());
    }

    fn matches(&self, placement: &PlacementRecord) -> bool {
        self.0.get(&placement.vmid) == Some(placement)
    }

    fn invalidate(&mut self, vmid: Ulid) {
        self.0.remove(&vmid);
    }
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
    /// Exact, in-process teardown acknowledgements retained for finalization retries.
    confirmed_stops: ConfirmedStopFences,
    // pub network_actor: ActorRef<NetworkAgentActor>,
    pub metadata: ObjectMetadata,
    pub state_store: Arc<StateStore>,
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

    async fn validate_vm_state(
        &self,
        vmid: Ulid,
        expected: &PlacementRecord,
        lifecycle: PlacementLifecycle,
        manifest: Option<&VmManifest>,
    ) -> std::result::Result<(), String> {
        let state = self
            .state_store
            .get_vm_state::<VmManifest>(vmid)
            .await
            .map_err(|error| format!("unable to read durable VM state: {error}"))?
            .ok_or_else(|| "durable VM state is missing".to_owned())?;
        if state.manifest.id != vmid
            || &state.placement != expected
            || state.placement.node != self.config.get_hostname()
            || state.placement.lifecycle != lifecycle
            || manifest.is_some_and(|manifest| manifest != &state.manifest)
        {
            return Err("durable VM owner or lifecycle does not authorize this request".to_owned());
        }
        Ok(())
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

    /// Registering with Kademlia can wait indefinitely when no peers are
    /// available. Keep it outside the agent's serial message handler so local
    /// VM creation and status polling continue in single-node deployments.
    fn register_vm_actor(actor_ref: ActorRef<VMActor>, vmid: Ulid) {
        tokio::spawn(async move {
            for name in [vm_actor_id(vmid), VM.to_owned()] {
                match tokio::time::timeout(
                    VM_ACTOR_REGISTRATION_TIMEOUT,
                    actor_ref.register(name.clone()),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => warn!(?error, %vmid, %name, "Unable to register VM actor"),
                    Err(_) => warn!(%vmid, %name, "Timed out registering VM actor"),
                }
            }
        });
    }
}

#[allow(clippy::unused_async_trait_impl)]
impl Actor for AgentActor {
    type Args = (Config, Arc<StateStore>);
    type Error = Report;

    async fn on_start(
        (config, state_store): Self::Args,
        actor_ref: ActorRef<Self>,
    ) -> Result<Self> {
        let peer_id = *actor_ref.id().peer_id().unwrap();

        info!(?peer_id, "Agent Actor started!");

        // spawn networking actor
        let network_actor: ActorRef<NetworkAgentActor> =
            NetworkAgentActor::spawn_link(&actor_ref, config.network.clone()).await;
        network_actor.register(NETWORK).await?;

        let sys = System::new_all();
        // Durable intent is not evidence that this process owns a live runtime.
        // Explicit creates attach actors after validating the exact active pair.
        let vms = AHashMap::new();
        let (used_vcpus, used_memory_bytes) = (0, 0);
        Ok(Self {
            vcpus: u32::try_from(sys.cpus().len()).unwrap_or(u32::MAX),
            memory: ByteSize::b(sys.total_memory()),
            config,
            vms,
            used_vcpus,
            used_memory_bytes,
            membership_revision: 0,
            status_history: StatusChangeHistory::new(),
            confirmed_stops: ConfirmedStopFences::default(),
            metadata: ObjectMetadata::default(),
            state_store,
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
        let Some(placement) = msg.placement.as_ref() else {
            return CreateVMReply {
                config: None,
                actor_id: None,
                error: Some("create is missing its durable placement".to_owned()),
            };
        };
        if let Err(error) = self
            .validate_vm_state(
                vmid,
                placement,
                PlacementLifecycle::Active,
                Some(&msg.config),
            )
            .await
        {
            return CreateVMReply {
                config: None,
                actor_id: None,
                error: Some(error),
            };
        }
        // A validated active placement is a new incarnation, so an older
        // teardown acknowledgement must no longer authorize a stop retry.
        self.confirmed_stops.invalidate(vmid);
        if let Some(existing) = self.vms.get(&vmid) {
            if existing.config != msg.config {
                return CreateVMReply {
                    config: None,
                    actor_id: None,
                    error: Some("conflicting create for cached VM".to_owned()),
                };
            }
            return CreateVMReply {
                config: Some(existing.config.clone()),
                actor_id: Some(existing.actor_ref.id().to_bytes()),
                error: None,
            };
        }

        let actor_ref =
            VMActor::spawn_link(ctx.actor_ref(), (vmid, Some(msg.config.clone()))).await;
        if let Err(error) = actor_ref
            .wait_for_startup_with_result(|result| result.map_err(|error| error.to_string()))
            .await
        {
            warn!(%error, %vmid, "Unable to start VM actor");
            return CreateVMReply {
                config: None,
                actor_id: None,
                error: Some(error),
            };
        }

        Self::register_vm_actor(actor_ref.clone(), vmid);
        self.insert_vm(
            vmid,
            VMCacheData {
                actor_ref: actor_ref.clone(),
                config: msg.config.clone(),
                vcpus: msg.config.desired.compute.vcpus,
                memory_bytes: msg.config.desired.compute.memory_bytes,
            },
        );
        info!(?vmid, "VM Spawned successfully");
        CreateVMReply {
            config: Some(msg.config),
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
        let actor_ref = VMActor::spawn_link(ctx.actor_ref(), (vmid, None)).await;

        Self::register_vm_actor(actor_ref.clone(), vmid);
        self.insert_vm(
            vmid,
            VMCacheData {
                actor_ref: actor_ref.clone(),
                config: msg.config.clone(),
                vcpus: msg.config.desired.compute.vcpus,
                memory_bytes: msg.config.desired.compute.memory_bytes,
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
            if self
                .vms
                .get(&vmid)
                .is_some_and(|cache| cache.actor_ref.id() == actor_ref.id())
            {
                self.remove_vm(vmid);
            }
            actor_ref.kill();
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
        let Some(placement) = msg.placement.as_ref() else {
            return DeleteVMReply {
                error: Some("delete is missing its durable stop fence".to_owned()),
            };
        };
        if let Err(error) = self
            .validate_vm_state(msg.vmid, placement, PlacementLifecycle::Deleting, None)
            .await
        {
            return DeleteVMReply { error: Some(error) };
        }
        let Some(actor_ref) = self.vms.get(&msg.vmid).map(|vm| vm.actor_ref.clone()) else {
            return if self.confirmed_stops.matches(placement) {
                DeleteVMReply { error: None }
            } else {
                DeleteVMReply {
                    error: Some("VM runtime is not cached; teardown is unconfirmed".to_owned()),
                }
            };
        };
        match tokio::time::timeout(VM_ACTOR_DELETE_TIMEOUT, actor_ref.ask(msg.clone())).await {
            Ok(Ok(reply)) => {
                if reply.error.is_none() {
                    self.remove_vm(msg.vmid);
                    self.confirmed_stops.confirm(placement);
                }
                reply
            }
            Ok(Err(error)) => DeleteVMReply {
                error: Some(error.to_string()),
            },
            Err(_) => DeleteVMReply {
                error: Some("timed out waiting for VM actor delete".to_owned()),
            },
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
        let Some(placement) = msg.placement.as_ref() else {
            return Err("shutdown is missing its durable stop fence".to_owned());
        };
        self.validate_vm_state(msg.vmid, placement, PlacementLifecycle::Stopping, None)
            .await?;
        let Some(actor_ref) = self.vms.get(&msg.vmid).map(|vm| vm.actor_ref.clone()) else {
            return if self.confirmed_stops.matches(placement) {
                Ok(ShutdownVMReply { error: None })
            } else {
                Err("VM runtime is not cached; teardown is unconfirmed".to_owned())
            };
        };
        trace!(?msg, "Requesting VM shutdown");
        match tokio::time::timeout(Duration::from_secs(30), actor_ref.ask(msg.clone())).await {
            Ok(Ok(reply)) if reply.error.is_none() => {
                self.remove_vm(msg.vmid);
                self.confirmed_stops.confirm(placement);
                Ok(reply)
            }
            Ok(Ok(reply)) => Err(reply
                .error
                .unwrap_or_else(|| "VM shutdown failed".to_owned())),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("timed out waiting for VM actor shutdown".to_owned()),
        }
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

        let Some(actor_ref) = self.vms.get(&vmid).map(|vm| vm.actor_ref.clone()) else {
            warn!(vm_id = %vmid, "VM actor not cached for info lookup");
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
mod tests {
    use super::ConfirmedStopFences;
    use crate::cluster_state::{
        MemoryStateStore, PlacementLifecycle, PlacementRecord, StateError, StateStore, StopIntent,
    };
    use ulid::Ulid;

    #[tokio::test]
    async fn confirmed_teardown_fence_retries_finalization_but_not_a_new_generation() {
        let store = StateStore::Memory(MemoryStateStore::default());
        let vmid = Ulid::generate();
        let manifest = serde_json::json!({"id": vmid});
        let active = PlacementRecord {
            vmid,
            node: "node-a".to_owned(),
            generation: Some(Ulid::generate()),
            lifecycle: PlacementLifecycle::Active,
        };
        store
            .create_vm_state(vmid, &manifest, &active)
            .await
            .unwrap();
        let stop = store.begin_vm_stop(vmid, StopIntent::Delete).await.unwrap();

        let mut fences = ConfirmedStopFences::default();
        assert!(!fences.matches(&stop));
        // The VM actor acknowledged teardown. A failed/stale store finalization
        // leaves this exact owner/generation/lifecycle fence for a retry.
        fences.confirm(&stop);
        let mut stale = stop.clone();
        stale.generation = Some(Ulid::generate());
        assert!(matches!(
            store.complete_vm_stop(&stale).await,
            Err(StateError::Conflict)
        ));
        assert!(fences.matches(&stop));
        store.complete_vm_stop(&stop).await.unwrap();

        let mut next_incarnation = active;
        next_incarnation.generation = Some(Ulid::generate());
        store
            .create_vm_state(vmid, &manifest, &next_incarnation)
            .await
            .unwrap();
        fences.invalidate(vmid);
        let next_stop = store.begin_vm_stop(vmid, StopIntent::Delete).await.unwrap();
        assert!(!fences.matches(&next_stop));
        assert!(matches!(
            store.complete_vm_stop(&stop).await,
            Err(StateError::Conflict)
        ));
        assert!(
            store
                .get_vm_state::<serde_json::Value>(vmid)
                .await
                .unwrap()
                .is_some()
        );
        drop(store);
    }
}
