//! VM management API handlers.
use crate::{
    actors::http_actor::HTTPActor,
    types::{
        CreateVMRequest, DebugCreateVMRequest, UpdateVMRequest, VMData, VirtualMachine, VMListResponse, VMStatus, VmId
    }, messages::vm::CreateVM, utils::OdoroboError,
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
use crate::messages::vm::{AgentListVMs, DeleteVM, ShutdownVM};

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

async fn list_vms(State(state): State<ActorRef<HTTPActor>>) -> Result<impl IntoApiResponse, OdoroboError> {
    let reply = state.ask(AgentListVMs).await?;

    Ok(Json(VMListResponse {
        vms: reply.vms.into_iter().map(VmId).collect(),
    }))
}

/// Get detailed information about a specific VM
async fn vm_info(
    State(_state): State<ActorRef<HTTPActor>>,
    Path(VmId(_vmid)): Path<VmId>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    // stub,
    Ok(Json(VirtualMachine::default()))
}

async fn create_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Json(request): Json<CreateVMRequest>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let vm_data = request.data.clone();
    let message = HTTPActor::create_vm_message(request);

    let _reply = state.ask(message).await?;

    Ok(Json(VirtualMachine {
        data: vm_data,
        node: None,
        status: VMStatus::Provisioning,
        ..Default::default()
    }))
}

async fn debug_create_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Json(request): Json<DebugCreateVMRequest>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let ulid = ulid::Ulid::new();
    let message = CreateVM {
        vmid: ulid,
        config: request.vm_config,
    };

    let _reply = state.ask(message).await?;

    Ok(Json(VirtualMachine {
        status: VMStatus::Provisioning,
        data: VMData {
            id: ulid,
            ..Default::default()
        },
        ..Default::default()
    }))
}

async fn delete_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Path(VmId(vmid)): Path<VmId>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let reply = state.ask(DeleteVM { vmid }).await?;

    Ok(Json(()))
}

async fn shutdown_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Path(VmId(vmid)): Path<VmId>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let _reply = state.ask(ShutdownVM { vmid }).await?;

    Ok(Json(()))
}

/// Update an existing VM's configuration (e.g. resize, change resources, etc.)
///
/// todo: make new schema for update request that allows partial updates
async fn update_vm(
    State(_state): State<ActorRef<HTTPActor>>,
    Path(VmId(_vmid)): Path<VmId>,
    Json(_request): Json<UpdateVMRequest>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    // stub

    Ok(Json(VirtualMachine::default()))
}
