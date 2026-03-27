use axum::{Json, extract::{Path, Query}};
use cloud_hypervisor_client::models::{self, VmInfo, VmmPingResponse};
use serde::Deserialize;
use stable_eyre::Result;

use super::error::ApiError;
use crate::state::VMInstance;

pub fn router() -> axum::Router<()> {
    axum::Router::new()
        .route("/", axum::routing::get(list_vms))
        .route("/", axum::routing::put(create_vm))
        .route("/{vmid}", axum::routing::get(vm_info))
        .route("/{vmid}/ping", axum::routing::get(ping_vm))
        .route("/{vmid}", axum::routing::delete(destroy_vm))
        .route("/{vmid}/console", axum::routing::get(super::console::console_stream))
}

async fn list_vms() -> Result<Json<Vec<String>>, ApiError> {
    let vms = VMInstance::list().map_err(|_| ApiError::ListFailed)?;
    Ok(Json(vms.into_iter().map(|i| i.id).collect()))
}

fn get_vm(vmid: &str) -> Result<VMInstance, ApiError> {
    VMInstance::get(vmid).ok_or_else(|| ApiError::VmNotFound(vmid.to_string()))
}

async fn vm_info(
    vmid: Path<String>,
) -> Result<Json<cloud_hypervisor_client::models::VmInfo>, ApiError> {
    let vm = get_vm(&vmid.0)?;

    let info = vm.info().await.map_err(|_| ApiError::VmInfoFailed)?;
    Ok(Json(info))
}
/// Pings the VMM to check if it's running
async fn ping_vm(vmid: Path<String>) -> Result<Json<VmmPingResponse>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    let res = vm.ping().await.map_err(|_| ApiError::VmInfoFailed)?;
    Ok(Json(res))
}

#[derive(Debug, Deserialize)]
pub struct CreateVmQuery {
    #[serde(default)]
    /// Whether to boot the VM immediately
    /// after creation
    ///
    /// Defaults to `false`.
    pub boot: bool,
}

/// Create a new VM
async fn create_vm(
    vmid: Path<String>,
    Query(query): Query<CreateVmQuery>,
    Json(vm_config): Json<models::VmConfig>,
) -> Result<Json<VmInfo>, ApiError> {
    let runtime_dir = VMInstance::runtime_dir_for(&vmid.0);
    std::fs::create_dir_all(&runtime_dir).map_err(|_| ApiError::CreateFailed)?;

    let socket_path = runtime_dir.join("ch.sock");
    let vm = VMInstance::new(&vmid.0, socket_path);
    vm.create(vm_config, query.boot)
        .await
        .map_err(|_| ApiError::CreateFailed)?;

    let info = vm.info().await.map_err(|_| ApiError::VmInfoFailed)?;
    Ok(Json(info))
}

async fn destroy_vm(vmid: Path<String>) -> Result<Json<()>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    vm.destroy().await.map_err(|_| ApiError::VmInfoFailed)?;
    Ok(Json(()))
}
