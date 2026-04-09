//! VM management API handlers.
use crate::{
    actors::http_actor::HTTPActor,
    api::types::{
        CreateVMRequest, DebugCreateVMRequest, UpdateVMRequest, VMInfo, VMListResponse, VmId,
    },
};
use aide::axum::{
    ApiRouter, IntoApiResponse,
    routing::{delete, get, patch, post, put},
};
use axum::{
    Json,
    extract::{Path, State},
};
use kameo::actor::ActorRef;
use odorobo_shared::messages::create_vm::{AgentListVMs, DeleteVM, ShutdownVM};

pub fn router() -> ApiRouter<ActorRef<HTTPActor>> {
    ApiRouter::new()
        .api_route("/", get(list_vms))
        .api_route("/", post(create_vm))
        // undocumented debug route, do not use in prod
        .route("/", axum::routing::put(debug_create_vm))
        .api_route("/{vmid}", get(vm_info))
        .api_route("/{vmid}", patch(update_vm))
        .api_route("/{vmid}", delete(delete_vm))
        .api_route("/{vmid}/shutdown", put(shutdown_vm))
}

async fn list_vms(State(state): State<ActorRef<HTTPActor>>) -> impl IntoApiResponse {
    let reply = state.ask(AgentListVMs).await.unwrap();

    Json(VMListResponse {
        vms: reply.vms.into_iter().map(VmId).collect(),
    })
}

/// Get detailed information about a specific VM
async fn vm_info(
    State(state): State<ActorRef<HTTPActor>>,
    Path(VmId(vmid)): Path<VmId>,
) -> impl IntoApiResponse {
    // stub,
    Json(VMInfo::default())
}

async fn create_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Json(request): Json<CreateVMRequest>,
) -> impl IntoApiResponse {
    let vm_data = request.data.clone();
    let message = HTTPActor::create_vm_message(request);

    let _reply = state.ask(message).await.unwrap();

    Json(VMInfo {
        data: vm_data,
        node: None,
        status: crate::api::types::VMStatus::Provisioning,
    })
}

async fn debug_create_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Json(request): Json<DebugCreateVMRequest>,
) -> impl IntoApiResponse {
    let message = odorobo_shared::messages::create_vm::CreateVM {
        vmid: ulid::Ulid::new(),
        config: request.vm_config,
    };

    let _reply = state.ask(message).await.unwrap();

    Json(VMInfo::default())
}

async fn delete_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Path(VmId(vmid)): Path<VmId>,
) -> impl IntoApiResponse {
    let _reply = state.ask(DeleteVM { vmid }).await.unwrap();

    Json(())
}

async fn shutdown_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Path(VmId(vmid)): Path<VmId>,
) -> impl IntoApiResponse {
    let _reply = state.ask(ShutdownVM { vmid }).await.unwrap();

    Json(())
}

/// Update an existing VM's configuration (e.g. resize, change resources, etc.)
///
/// todo: make new schema for update request that allows partial updates
async fn update_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Path(VmId(vmid)): Path<VmId>,
    Json(request): Json<UpdateVMRequest>,
) -> impl IntoApiResponse {
    // stub

    Json(VMInfo::default())
}
