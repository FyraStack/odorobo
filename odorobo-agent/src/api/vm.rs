use axum::{
    Json,
    extract::{Path, Query},
};
use cloud_hypervisor_client::models::{self, VmInfo, VmmPingResponse};
use serde::{Deserialize, Serialize};
use stable_eyre::Result;
use tracing::{error, trace};

use super::error::ApiError;
use crate::state::VMInstance;

pub fn router() -> axum::Router<()> {
    axum::Router::new()
        .route("/", axum::routing::get(list_vms))
        .route("/{vmid}", axum::routing::put(spawn_vm))
        .route("/{vmid}/config", axum::routing::put(create_vm_config))
        .route("/{vmid}/config", axum::routing::delete(delete_vm_config))
        .route("/{vmid}/shutdown", axum::routing::put(shutdown_vm))
        .route("/{vmid}/acpi_shutdown", axum::routing::put(shutdown_acpi))
        .route("/{vmid}/boot", axum::routing::put(boot_vm))
        .route("/{vmid}/pause", axum::routing::put(pause_vm))
        .route("/{vmid}/resume", axum::routing::put(resume_vm))
        .route("/{vmid}", axum::routing::get(vm_info))
        .route("/{vmid}/ping", axum::routing::get(ping_vm))
        .route("/{vmid}", axum::routing::delete(destroy_vm))
        .route(
            "/{vmid}/console",
            axum::routing::get(super::console::console_stream),
        )
        .route(
            "/{vmid}/ch/{*path}",
            axum::routing::any(super::ch::passthrough),
        )
}

/// Lists all VMs by their IDs
async fn list_vms() -> Result<Json<Vec<String>>, ApiError> {
    let vms = VMInstance::list().map_err(|e| ApiError::ListFailed { msg: e.to_string() })?;
    Ok(Json(vms.into_iter().map(|i| i.id).collect()))
}

/// Helper function to get a VM instance by ID, returning an error if not found
fn get_vm(vmid: &str) -> Result<VMInstance, ApiError> {
    use crate::state::VMInstance;
    VMInstance::validate_vmid(vmid).map_err(|e| ApiError::InvalidVmId { msg: e.to_string() })?;
    VMInstance::get(vmid).ok_or_else(|| ApiError::VmNotFound {
        vmid: vmid.to_string(),
    })
}

/// Gets detailed information about a specific VM
async fn vm_info(
    vmid: Path<String>,
) -> Result<Json<cloud_hypervisor_client::models::VmInfo>, ApiError> {
    let vm = get_vm(&vmid.0)?;

    let info = vm.info().await.map_err(ApiError::vm_info)?;
    Ok(Json(info))
}

/// Pings the VMM to check if it's running
async fn ping_vm(vmid: Path<String>) -> Result<Json<VmmPingResponse>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    let res = vm.ping().await.map_err(ApiError::vm_info)?;
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
#[derive(Debug, Deserialize, Serialize)]
pub struct VmSpawnResponse {
    pub info: Option<VmInfo>,
    pub booted: bool,
    pub created_config: bool,
}

/// Spawns a new VM instance with the given ID, optionally creating it with the provided configuration and booting it immediately
async fn spawn_vm(
    vmid: Path<String>,
    Query(query): Query<CreateVmQuery>,
    vm_config: Option<Json<models::VmConfig>>,
) -> Result<Json<VmSpawnResponse>, ApiError> {
    VMInstance::validate_vmid(&vmid.0).map_err(|e| ApiError::InvalidVmId { msg: e.to_string() })?;
    let vm_config = vm_config.map(|Json(vm_config)| vm_config);

    // check if VM already exists, if so error out for already existing instance
    if VMInstance::get(&vmid.0).is_some() {
        error!(vmid = ?vmid, "VM with this ID already exists");
        return Err(ApiError::CreateFailed {
            msg: "VM with this ID already exists".to_string(),
            errors: vec![],
        });
    }

    trace!(?vmid, ?query, "Creating VM with config");
    // trace!(?vm_config, "VM config details");
    let runtime_dir = VMInstance::runtime_dir_for(&vmid.0);
    std::fs::create_dir_all(&runtime_dir).map_err(|e| {
        error!(error = %e, "Failed to create runtime dir");
        ApiError::CreateFailed {
            msg: e.to_string(),
            errors: vec![],
        }
    })?;
    // trace!(?)

    let vm = VMInstance::spawn(&vmid.0).await.map_err(|e| {
        error!(error = ?e, "Failed to spawn VM process");
        ApiError::create(e)
    })?;

    let mut created = false;
    if vm_config.is_some() {
        trace!(?vmid, "Creating VM with provided config");
        vm.create(vm_config.clone().unwrap(), query.boot)
            .await
            .map_err(|e| {
                error!(error = ?e, "Failed to create VM");
                ApiError::create_config(e)
            })?;

        created = true;
    } else {
        trace!(?vmid, "No VM config provided, skipping creation step");
    }

    let vm_info = if vm_config.is_some() {
        Some(vm.info().await.map_err(|e| {
            error!(error = ?e, "Failed to get VM info after creation");
            ApiError::vm_info(e)
        })?)
    } else {
        None
    };

    Ok(Json(VmSpawnResponse {
        info: vm_info,
        booted: query.boot,
        created_config: created,
    }))
}

async fn create_vm_config(
    vmid: Path<String>,
    Query(query): Query<CreateVmQuery>,
    Json(vm_config): Json<models::VmConfig>,
) -> Result<Json<()>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    vm.create(vm_config, query.boot)
        .await
        .map_err(ApiError::create_config)?;
    Ok(Json(()))
}

async fn delete_vm_config(vmid: Path<String>) -> Result<Json<()>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    vm.delete_config()
        .await
        .map_err(ApiError::delete_config)?;
    Ok(Json(()))
}

/// Forces a VM to shut down immediately, without giving it a chance to gracefully clean up resources.
/// This is equivalent to pulling the power on a physical machine and may lead to data loss or corruption if the VM is running.
///
/// The VM process itself will still be running until the VMM detects that the VM has stopped,
/// not fully cleaning up resources.
async fn shutdown_vm(vmid: Path<String>) -> Result<Json<()>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    vm.shutdown().await.map_err(ApiError::vm_info)?;
    Ok(Json(()))
}

/// Sends an ACPI shutdown signal to the VM, allowing it to gracefully shut down and clean up resources.
///
/// With the systemd provisioner, this will also fully clean up resources and destroy the VM instance entirely,
/// allowing them to be re-provisioned again on any other node (if running in a cluster)
async fn shutdown_acpi(vmid: Path<String>) -> Result<Json<()>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    vm.acpi_power_button()
        .await
        .map_err(ApiError::vm_info)?;
    Ok(Json(()))
}

/// Boots a VM that has been created but not yet started. If the VM is already running, this will return an error.
async fn boot_vm(vmid: Path<String>) -> Result<Json<()>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    vm.boot().await.map_err(ApiError::vm_info)?;
    Ok(Json(()))
}

/// Suspends a running VM, pausing all activity until
/// it is resumed again.
async fn pause_vm(vmid: Path<String>) -> Result<Json<()>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    vm.pause().await.map_err(ApiError::vm_info)?;
    Ok(Json(()))
}

/// Resumes a paused VM, allowing it to continue running from where it left off.
async fn resume_vm(vmid: Path<String>) -> Result<Json<()>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    vm.resume().await.map_err(ApiError::vm_info)?;
    Ok(Json(()))
}

/// Destroys a VM, stopping it if it's running and cleaning up resources
async fn destroy_vm(vmid: Path<String>) -> Result<Json<()>, ApiError> {
    let vm = get_vm(&vmid.0)?;
    vm.destroy().await.map_err(ApiError::vm_info)?;
    Ok(Json(()))
}
