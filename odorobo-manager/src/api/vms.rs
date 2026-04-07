//! VM management API handlers.
use crate::api::types::{CreateVMRequest, UpdateVMRequest, VMInfo, VmId};
use aide::axum::{
    ApiRouter, IntoApiResponse,
    routing::{delete, get, patch, post},
};
use axum::{Json, extract::Path};

pub fn router() -> ApiRouter {
    ApiRouter::new()
        .api_route("/", post(create_vm))
        .api_route("/{vmid}", get(vm_info))
        .api_route("/{vmid}", patch(update_vm))
        .api_route("/{vmid}", delete(delete_vm))
}

/// Get detailed information about a specific VM
async fn vm_info(Path(VmId(vmid)): Path<VmId>) -> impl IntoApiResponse {
    // stub,
    Json(VMInfo::default())
}

async fn create_vm(Json(request): Json<CreateVMRequest>) -> impl IntoApiResponse {
    // stub
    Json(VMInfo::default())
}

async fn delete_vm(Path(VmId(vmid)): Path<VmId>) -> impl IntoApiResponse {
    // stub
}

/// Update an existing VM's configuration (e.g. resize, change resources, etc.)
///
/// todo: make new schema for update request that allows partial updates
async fn update_vm(
    Path(VmId(vmid)): Path<VmId>,
    Json(request): Json<UpdateVMRequest>,
) -> impl IntoApiResponse {
    // stub

    Json(VMInfo::default())
}
