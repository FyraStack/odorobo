use crate::{
    ch_driver::actor::VMActor,
    config::Config,
    messages::{
        Ping, Pong,
        agent::{AgentStatus, GetAgentStatus},
        debug::PanicAgent,
        vm::{
            AgentListVMs, AgentListVMsReply, CreateVM, CreateVMReply, DeleteVM, DeleteVMReply,
            GetVMInfo, GetVMInfoReply, MigrateVMReceive, MigrateVMReceiveReply, ShutdownVM,
            ShutdownVMReply,
        },
    },
    networking::actor::NetworkAgentActor,
    types::{ObjectMetadata, VirtualMachine},
    utils::actor_names::{NETWORK, VM, vm_actor_id},
};
use ahash::AHashMap;
use bytesize::ByteSize;
use kameo::prelude::*;
use odorobo::cluster_state::{ClusterStateStore, StateStore, VM_MANIFESTS_PREFIX, key};
use stable_eyre::{Report, Result};
use std::{ops::ControlFlow, sync::Arc};
use sysinfo::System;
use tracing::{error, info, trace, warn};
use ulid::Ulid;

use kameo::error::PanicError;

pub struct VMCacheData {
    actor_ref: ActorRef<VMActor>,
    config: VirtualMachine,
}

#[derive(RemoteActor)]
pub struct AgentActor {
    pub vcpus: u32,
    pub memory: ByteSize,
    pub config: Config,
    pub vms: AHashMap<Ulid, VMCacheData>,
    // pub network_actor: ActorRef<NetworkAgentActor>,
    pub metadata: ObjectMetadata,
    pub state_store: Arc<StateStore>,
}

impl AgentActor {
    async fn lookup_vm_actor(vmid: Ulid) -> Option<ActorRef<VMActor>> {
        ActorRef::<VMActor>::lookup(format!("vm:{vmid}"))
            .await
            .ok()
            .flatten()
    }
}

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
        match state_store
            .list::<VirtualMachine>(VM_MANIFESTS_PREFIX)
            .await
        {
            Ok(records) => {
                for (_, vm_config) in records {
                    let vmid = vm_config.data.id;
                    let actor =
                        VMActor::spawn_link(&actor_ref, (vmid, Some(vm_config.clone()))).await;
                    _ = actor.register(vm_actor_id(vmid)).await;
                    _ = actor.register(VM).await;
                    vms.insert(
                        vmid,
                        VMCacheData {
                            actor_ref: actor,
                            config: vm_config,
                        },
                    );
                }
                info!(
                    count = vms.len(),
                    "Recovered VM manifests from cluster state"
                );
            }
            Err(error) => warn!(
                ?error,
                "Unable to recover VM manifests; retaining empty local cache"
            ),
        }

        Ok(Self {
            vcpus: u32::try_from(sys.cpus().len()).unwrap_or(u32::MAX),
            memory: ByteSize::b(sys.total_memory()),
            config,
            vms,
            metadata: ObjectMetadata::default(),
            state_store,
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

        self.vms.retain(|_, vm| vm.actor_ref.id() != id);

        Ok(ControlFlow::Continue(()))
    }
}

#[remote_message]
impl Message<CreateVM> for AgentActor {
    type Reply = CreateVMReply;

    async fn handle(&mut self, msg: CreateVM, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let vmid = msg.vmid;
        if let Err(error) = self
            .state_store
            .put(&key(VM_MANIFESTS_PREFIX, &vmid), &msg.config)
            .await
        {
            warn!(?error, vm_id = %vmid, "Unable to persist VM manifest; continuing without durable state");
        }
        // spawn AND link at the same time
        let actor_ref =
            VMActor::spawn_link(ctx.actor_ref(), (vmid, Some(msg.config.clone()))).await;

        _ = actor_ref.register(vm_actor_id(vmid)).await;
        _ = actor_ref.register(VM).await;
        self.vms.insert(
            vmid,
            VMCacheData {
                actor_ref: actor_ref.clone(),
                config: msg.config.clone(),
            },
        );

        info!(?vmid, "VM Spawned successfully");
        CreateVMReply {
            config: Some(msg.config),
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
        self.vms.insert(
            vmid,
            VMCacheData {
                actor_ref: actor_ref.clone(),
                config: VirtualMachine::default(),
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
        match self.vms.remove(&msg.vmid) {
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

        if let Err(error) = self
            .state_store
            .delete(&key(VM_MANIFESTS_PREFIX, &msg.vmid))
            .await
        {
            warn!(?error, vm_id = %msg.vmid, "Unable to delete VM manifest; retaining durable record for recovery");
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
impl Message<GetAgentStatus> for AgentActor {
    type Reply = AgentStatus;

    async fn handle(
        &mut self,
        _msg: GetAgentStatus,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let vcpus_used_by_vms = self
            .vms
            .values()
            .map(|vm| vm.config.data.vcpus)
            .reduce(u32::saturating_add)
            .unwrap_or(0);

        let ram_used_by_vms = self
            .vms
            .values()
            .map(|vm| vm.config.data.memory.as_u64())
            .reduce(u64::saturating_add)
            .unwrap_or(0);

        AgentStatus {
            hostname: self.config.get_hostname().to_owned(),
            vcpus: self.vcpus,
            ram: self.memory,
            vms: self.vms.keys().copied().collect(),
            used_vcpus: vcpus_used_by_vms.saturating_add(self.config.get_reserved_vcpus()),
            used_ram: ByteSize::b(ram_used_by_vms),
        }
    }
}
