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
use ahash::{AHashMap, AHashSet};
use bytesize::ByteSize;
use kameo::prelude::*;
use odorobo::cluster_state::{
    ClusterStateStore, PLACEMENT_PREFIX, PlacementRecord, StateStore, VM_MANIFESTS_PREFIX,
};
use stable_eyre::{Report, Result, eyre::eyre};
use std::{ops::ControlFlow, sync::Arc, time::Duration};
use sysinfo::System;
use tracing::{error, info, trace, warn};
use ulid::Ulid;

use kameo::error::PanicError;

pub struct VMCacheData {
    actor_ref: ActorRef<VMActor>,
    config: VmManifest,
    vcpus: u32,
    memory_bytes: u64,
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
    // pub network_actor: ActorRef<NetworkAgentActor>,
    pub metadata: ObjectMetadata,
    pub state_store: Arc<StateStore>,
}

impl AgentActor {
    fn manifests_for_node(
        placements: Vec<(String, PlacementRecord)>,
        records: Vec<(String, VmManifest)>,
        hostname: &str,
    ) -> Vec<VmManifest> {
        let local_vmids: AHashSet<_> = placements
            .into_iter()
            .map(|(_, placement)| placement)
            .filter(|placement| placement.node == hostname)
            .map(|placement| placement.vmid)
            .collect();
        records
            .into_iter()
            .map(|(_, manifest)| manifest)
            .filter(|manifest| local_vmids.contains(&manifest.id))
            .collect()
    }

    #[cfg(test)]
    fn recovered_resources(manifests: &[VmManifest]) -> (u32, u64) {
        manifests
            .iter()
            .fold((0, 0), |(vcpus, memory_bytes), manifest| {
                (
                    vcpus.saturating_add(manifest.desired.compute.vcpus),
                    memory_bytes.saturating_add(manifest.desired.compute.memory_bytes),
                )
            })
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

    async fn lookup_vm_actor(vmid: Ulid) -> Option<ActorRef<VMActor>> {
        ActorRef::<VMActor>::lookup(format!("vm:{vmid}"))
            .await
            .ok()
            .flatten()
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
        let mut vms = AHashMap::new();
        let placements = state_store
            .list::<PlacementRecord>(PLACEMENT_PREFIX)
            .await
            .map_err(|error| eyre!("unable to recover VM placements: {error}"))?;
        let records = state_store
            .list::<VmManifest>(VM_MANIFESTS_PREFIX)
            .await
            .map_err(|error| eyre!("unable to recover VM manifests: {error}"))?;
        let recovered_manifests =
            Self::manifests_for_node(placements, records, config.get_hostname());
        for vm_config in recovered_manifests {
            let vmid = vm_config.id;
            if let Some(actor) = Self::lookup_vm_actor(vmid).await {
                vms.insert(
                    vmid,
                    VMCacheData {
                        actor_ref: actor,
                        config: vm_config.clone(),
                        vcpus: vm_config.desired.compute.vcpus,
                        memory_bytes: vm_config.desired.compute.memory_bytes,
                    },
                );
                continue;
            }
            let actor = VMActor::spawn_link(&actor_ref, (vmid, Some(vm_config.clone()))).await;
            let startup = actor
                .wait_for_startup_with_result(|result| result.map_err(|error| error.to_string()))
                .await;
            if let Err(error) = startup {
                warn!(%error, %vmid, "Unable to start recovered VM actor");
                continue;
            }
            if let Err(error) = actor.register(vm_actor_id(vmid)).await {
                error!(?error, %vmid, "Unable to register recovered VM actor");
                actor.kill();
                continue;
            }
            if let Err(error) = actor.register(VM).await {
                error!(?error, %vmid, "Unable to register recovered VM actor group");
                actor.kill();
                continue;
            }
            vms.insert(
                vmid,
                VMCacheData {
                    actor_ref: actor,
                    config: vm_config.clone(),
                    vcpus: vm_config.desired.compute.vcpus,
                    memory_bytes: vm_config.desired.compute.memory_bytes,
                },
            );
        }
        let (used_vcpus, used_memory_bytes) =
            vms.values()
                .fold((0_u32, 0_u64), |(vcpus, memory_bytes), vm| {
                    (
                        vcpus.saturating_add(vm.vcpus),
                        memory_bytes.saturating_add(vm.memory_bytes),
                    )
                });
        info!(
            count = vms.len(),
            "Recovered VM manifests from cluster state"
        );

        Ok(Self {
            vcpus: u32::try_from(sys.cpus().len()).unwrap_or(u32::MAX),
            memory: ByteSize::b(sys.total_memory()),
            config,
            vms,
            used_vcpus,
            used_memory_bytes,
            membership_revision: 0,
            status_history: StatusChangeHistory::new(),
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
        if let Some(existing) = self.vms.get(&vmid) {
            let actor_ref = existing.actor_ref.clone();
            if tokio::time::timeout(
                Duration::from_secs(5),
                actor_ref.ask(crate::messages::vm::GetVMHeartbeat),
            )
            .await
            .is_ok_and(|result| result.is_ok_and(|reply| reply.error.is_none()))
            {
                if existing.config == msg.config {
                    info!(?vmid, actor_id = ?actor_ref.id(), "VM already exists; treating create as idempotent");
                } else {
                    warn!(?vmid, actor_id = ?actor_ref.id(), "Ignoring conflicting create for existing VM");
                }
                return CreateVMReply {
                    config: Some(existing.config.clone()),
                    actor_id: Some(actor_ref.id().to_bytes()),
                };
            }
            if tokio::time::timeout(Duration::from_secs(5), actor_ref.wait_for_shutdown())
                .await
                .is_err()
            {
                warn!(
                    ?vmid,
                    "VM actor did not stop after a failed heartbeat; force killing it"
                );
                actor_ref.kill();
            }
            self.remove_vm(vmid);
        }
        if let Some(actor_ref) = Self::lookup_vm_actor(vmid).await {
            let actor_id = actor_ref.id().to_bytes();
            self.insert_vm(
                vmid,
                VMCacheData {
                    actor_ref,
                    config: msg.config.clone(),
                    vcpus: msg.config.desired.compute.vcpus,
                    memory_bytes: msg.config.desired.compute.memory_bytes,
                },
            );
            return CreateVMReply {
                config: Some(msg.config),
                actor_id: Some(actor_id),
            };
        }

        // Spawn and link at the same time.
        let actor_ref =
            VMActor::spawn_link(ctx.actor_ref(), (vmid, Some(msg.config.clone()))).await;
        let startup = actor_ref
            .wait_for_startup_with_result(|result| result.map_err(|error| error.to_string()))
            .await;
        if let Err(error) = startup {
            warn!(%error, %vmid, "Unable to start VM actor");
            return CreateVMReply {
                config: None,
                actor_id: None,
            };
        }

        if let Err(error) = actor_ref.register(vm_actor_id(vmid)).await {
            error!(?error, %vmid, "Unable to register VM actor");
            actor_ref.kill();
            return CreateVMReply {
                config: None,
                actor_id: None,
            };
        }
        if let Err(error) = actor_ref.register(VM).await {
            error!(?error, %vmid, "Unable to register VM actor group");
            actor_ref.kill();
            return CreateVMReply {
                config: None,
                actor_id: None,
            };
        }
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

        _ = actor_ref.register(vm_actor_id(vmid)).await;
        _ = actor_ref.register(VM).await;
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
        match self.vms.get(&msg.vmid).map(|vm| vm.actor_ref.clone()) {
            Some(actor_ref) => match actor_ref.ask(msg.clone()).await {
                Ok(reply) => {
                    if reply.error.is_none() {
                        self.remove_vm(msg.vmid);
                    }
                    return reply;
                }
                Err(error) => {
                    warn!(vm_id = %msg.vmid, ?error, "failed to delete VM actor");
                    return DeleteVMReply {
                        error: Some(error.to_string()),
                    };
                }
            },
            None => {
                warn!(vm_id = %msg.vmid, "VM actor not found for delete");
            }
        }

        DeleteVMReply { error: None }
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
        let actor_ref = if let Some(vm) = self.vms.get(&msg.vmid) {
            Some(vm.actor_ref.clone())
        } else {
            Self::lookup_vm_actor(msg.vmid).await
        };
        let Some(actor_ref) = actor_ref else {
            warn!(vm_id = %msg.vmid, "VM actor not found for shutdown");
            return Err("VM actor not found for shutdown".to_owned());
        };

        trace!(?msg, "Requesting VM shutdown");
        match tokio::time::timeout(Duration::from_secs(30), actor_ref.ask(msg.clone())).await {
            Ok(Ok(())) => {
                self.remove_vm(msg.vmid);
                Ok(ShutdownVMReply)
            }
            Ok(Err(error)) => {
                warn!(vm_id = %msg.vmid, ?error, "failed to shutdown VM actor");
                Err(error.to_string())
            }
            Err(_) => {
                warn!(vm_id = %msg.vmid, "timed out waiting for VM actor shutdown");
                Err("timed out waiting for VM actor shutdown".to_owned())
            }
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
mod tests {
    use super::AgentActor;
    use crate::manifest::{Boot, Compute, DesiredState, MANIFEST_VERSION, Metadata, VmManifest};
    use odorobo::cluster_state::PlacementRecord;
    use ulid::Ulid;

    fn manifest(id: Ulid, vcpus: u32, memory_bytes: u64) -> VmManifest {
        VmManifest {
            api_version: MANIFEST_VERSION,
            id,
            desired: DesiredState {
                metadata: Metadata {
                    name: id.to_string(),
                    ..Default::default()
                },
                compute: Compute {
                    vcpus,
                    memory_bytes,
                    ..Default::default()
                },
                boot: Boot::default(),
                ..Default::default()
            },
            observed: None,
        }
    }

    #[test]
    fn recovery_selects_local_manifests_and_restores_resource_usage() {
        let local_id = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ULID");
        let remote_id = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAW").expect("valid ULID");
        let orphan_id = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAX").expect("valid ULID");
        let recovered = AgentActor::manifests_for_node(
            vec![
                (
                    "placement/local".to_owned(),
                    PlacementRecord {
                        vmid: local_id,
                        node: "node-a".to_owned(),
                    },
                ),
                (
                    "placement/remote".to_owned(),
                    PlacementRecord {
                        vmid: remote_id,
                        node: "node-b".to_owned(),
                    },
                ),
            ],
            vec![
                ("manifest/local".to_owned(), manifest(local_id, 2, 512)),
                ("manifest/remote".to_owned(), manifest(remote_id, 4, 1024)),
                ("manifest/orphan".to_owned(), manifest(orphan_id, 8, 2048)),
            ],
            "node-a",
        );

        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].id, local_id);
        assert_eq!(AgentActor::recovered_resources(&recovered), (2, 512));
    }
}
