//! VM management API handlers.
use crate::{
    actors::http_actor::HTTPActor,
    types::{
        CreateVMRequest, UpdateVMRequest, VMData, VirtualMachine, VMListResponse, VMStatus, VmId
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
    let message = CreateVM {
        vmid: request.vm.data.id,
        config: request.vm,
    };

    let reply = state.ask(message).await?;

    Ok(Json(reply))
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
