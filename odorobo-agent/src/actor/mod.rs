use crate::state::provisioning::actor::VMActor;
use ahash::AHashMap;
use bytesize::ByteSize;
use kameo::prelude::*;
use odorobo_shared::messages::create_vm::*;
use odorobo_shared::messages::debug::PanicAgent;
use serde::{Deserialize, Serialize};
use stable_eyre::{Report, Result};
use std::{fs, path::PathBuf};
use sysinfo::System;
use std::ops::ControlFlow;
use kameo::error::ActorStopReason;

use kameo::error::PanicError;

#[derive(RemoteActor)]
pub struct AgentActor {
    pub vcpus: u32,
    pub memory: ByteSize,
    pub config: Config,
}

/// Gets the system hostname
pub fn hostname() -> String {
    System::host_name().unwrap_or("odorobo".into())
}

/// This was requested by katherine. Do not change without asking her.
pub fn default_reserved_vcpus() -> u32 {
    2
}

// The infra team wants a config file on the box where they can set info specific for the box its on.
// TODO: Double check with infra team (katherine) if they want any other config on the box.
#[derive(Serialize, Deserialize)]
pub struct Config {
    /// The hostname of the agent. Defaults to the system hostname
    /// if not specified in the config file.
    #[serde(default = "hostname")]
    pub hostname: String,
    /// The datacenter the agent is running in.
    pub datacenter: String,
    /// The region the agent is running in.
    pub region: String,
    /// The number of VCPUs reserved for the agent. Defaults to 2.
    #[serde(default = "default_reserved_vcpus")]
    pub reserved_vcpus: u32,
    /// this is just arbitrary data that will be shown but does no config
    /// Arbitrary labels that can be used
    #[serde(default)]
    pub labels: AHashMap<String, String>,
    /// Arbitrary annotations that can be used
    #[serde(default)]
    pub annotations: AHashMap<String, String>,
}

impl Actor for AgentActor {
    type Args = ();
    type Error = Report;

    async fn on_start(state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self> {
        // TODO: ask infra team where they want this on the box
        let file = fs::File::open("config.json").expect("file should open read only");
        let config: Config = serde_json::from_reader(file).expect("file should be proper JSON");

        let sys = System::new_all();

        Ok(AgentActor {
            vcpus: sys.cpus().len() as u32,
            memory: ByteSize::b(sys.total_memory()),
            config,
        })
    }

    // async fn on_panic(state: Self::Args, weak_actor_ref: WeakActorRef<Self>, _panic: &PanicError) {
    //     panic!("Agent panicked: {:?}", _panic);
    // }
    // 
    async fn on_panic(
            &mut self,
            actor_ref: WeakActorRef<Self>,
            err: PanicError,
        ) -> Result<std::ops::ControlFlow<ActorStopReason>> {
        println!("Agent panicked: {:?}", err);
        
        // todo: if we panic, we should completely regen the self struct from scratch. The assumption should be that memory corruption could have possibly happened becauew
        
        Ok(ControlFlow::Continue(()))
    }
}

#[remote_message]
impl Message<CreateVM> for AgentActor {
    type Reply = CreateVMReply;

    async fn handle(
        &mut self,
        msg: CreateVM,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // TODO: this is unfinished. we intend on using the state::provisioning::actor stuff for this I think.
        let vmid = ulid::Ulid::new();
        let actor_ref = VMActor::spawn(VMActor {
            ch_socket_path: PathBuf::from(format!("/run/odorobo/vms/{}/ch.sock", vmid)),
            vmid,
            vm_config: Default::default(),
        });

        let actor_registration_result = actor_ref.register("vm").await;

        tracing::info!("someone asked us for available capacity");
        CreateVMReply {
            config: Default::default(),
        }
    }
}

#[remote_message]
impl Message<PanicAgent> for AgentActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: PanicAgent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        tracing::info!("panicking");
        panic!();
    }
}

#[cfg(test)]
mod tests {}
