use crate::{
    ch_driver::actor::VMActor,
    config::Config,
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
use std::ops::ControlFlow;
use sysinfo::System;
use tracing::{error, info, trace, warn};
use ulid::Ulid;

use kameo::error::PanicError;

pub struct VMCacheData {
    actor_ref: ActorRef<VMActor>,
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
            used_vcpus: 0,
            used_memory_bytes: 0,
            membership_revision: 0,
            status_history: StatusChangeHistory::new(),
            metadata: ObjectMetadata::default(),
        })
    }

    // async fn on_panic(state: Self::Args, weak_actor_ref: WeakActorRef<Self>, _panic: &PanicError) {
    //     panic!("Agent panicked: {:?}", _panic);
    // }
    //
    async fn on_panic(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        err: PanicError,
    ) -> Result<std::ops::ControlFlow<ActorStopReason>> {
        error!("Agent panicked: {:?}", err);

        // todo: if we panic, we should completely regen the self struct from scratch. The assumption should be that memory corruption could have possibly happened becauew

        Ok(ControlFlow::Continue(()))
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
        // spawn AND link at the same time
        let actor_ref =
            VMActor::spawn_link(ctx.actor_ref(), (vmid, Some(msg.config.clone()))).await;

        _ = actor_ref.register(vm_actor_id(vmid)).await;
        _ = actor_ref.register(VM).await;
        self.insert_vm(
            vmid,
            VMCacheData {
                actor_ref: actor_ref.clone(),
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
                vcpus: msg.config.desired.compute.vcpus,
                memory_bytes: msg.config.desired.compute.memory_bytes,
            },
        );

        // now ask the VM actor to handle the migration receive
        actor_ref
            .ask(msg)
            .await
            .expect("failed to start migration receiver on destination VM actor")
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
        match self.remove_vm(msg.vmid) {
            Some(cache_data) => {
                let res = cache_data.actor_ref.tell(msg.clone()).await;
                if let Err(err) = res {
                    // probably a bad way to do this
                    warn!(vm_id = %msg.vmid, ?err, "failed to stop VM actor gracefully, killing");
                    cache_data.actor_ref.kill();
                }
            }
            None => {
                warn!(vm_id = %msg.vmid, "VM actor not found for delete");
            }
        }

        DeleteVMReply
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
            vms: self.vms.keys().copied().collect(),
            used_vcpus,
            used_ram,
            metadata: self.metadata.clone(),
        };

        if msg.since_revision == 0
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

        let mut added = Vec::new();
        let mut removed = Vec::new();
        for change in self
            .status_history
            .iter()
            .filter(|change| change.revision > msg.since_revision)
        {
            if change.added {
                added.push(change.vmid);
            } else {
                removed.push(change.vmid);
            }
        }
        AgentStatusUpdate::Delta {
            revision: self.membership_revision,
            added,
            removed,
            used_vcpus,
            used_ram,
        }
    }
}
