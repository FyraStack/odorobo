//! Temporary state management for the agent.
//! in-memory state inside /run is not persisted across reboots, so we can use it to store runtime state of the agent and VMs like currently running
//! VMs, and their instances, etc.
//!
//! Persistent stuff should be stored in the database to reconcile from

use cloud_hypervisor_client::{
    SocketBasedApiClient,
    apis::{ApiError, DefaultApi, Error as ChApiClientError},
    models::{self, VmConfig, VmInfo, vm_info::State},
};
use http_body_util::BodyExt;
use hyper::{Method, Request, header::CONTENT_TYPE};
use hyperlocal::UnixClientExt;
use serde_json::Value;
use stable_eyre::{
    Result,
    eyre::{Context, eyre},
};
use std::{
    env,
    fs::{self},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};
use tracing::{info, trace, warn};

const DEFAULT_RUNTIME_ROOT_DIR: &str = "/run/odorobo";
const RUNTIME_ROOT_ENV_VAR: &str = "ODOROBO_RUNTIME_DIR";

pub trait VMStateManager: DefaultApi {
    fn vm_id(&self) -> &str;
    fn ch_socket_path(&self) -> &Path;
    fn config_path(&self) -> PathBuf;

    /// Get the current VM info for this VM instance, if it exists.
    async fn info(&self) -> Result<VmInfo> {
        self.vm_info_get().await.wrap_err(eyre!(
            "Failed to get VM info for VM with id {}",
            self.vm_id()
        ))
    }

    /// Fetch the running VM's config from the CH info API.
    async fn runtime_config(&self) -> Result<VmConfig> {
        let info = self.info().await?;
        Ok(info.config)
    }

    /// Fetch the desired VM config from disk.
    fn file_config(&self) -> Result<VmConfig> {
        let config_data = fs::read_to_string(self.config_path()).wrap_err(eyre!(
            "Failed to read config file for VM with id {}",
            self.vm_id()
        ))?;

        let config: VmConfig = serde_json::from_str(&config_data).wrap_err(eyre!(
            "Failed to parse config JSON for VM with id {}",
            self.vm_id()
        ))?;

        Ok(config)
    }

    /// Save the desired VM config to disk.
    fn save_config(&self, config: &VmConfig) -> Result<()> {
        let config_data =
            serde_json::to_string_pretty(config).wrap_err("Failed to serialize config to JSON")?;

        fs::write(self.config_path(), config_data).wrap_err(eyre!(
            "Failed to write config file for VM with id {}",
            self.vm_id()
        ))?;

        Ok(())
    }

    /// Apply/reconcile the given config to the running VM.
    async fn apply_config(&self, config: &VmConfig) -> Result<()> {
        self.save_config(config)?;

        let vm_state = match self.info().await {
            Ok(info) => {
                let vm_state = info.state;
                info!(
                    vm_id = self.vm_id(),
                    ?vm_state,
                    "Applying config for VM, current state of VM is {vm_state:?}",
                );

                vm_state
            }
            Err(err) => {
                if let Some(api_error) = find_api_error(&err) {
                    info!(vm_id = self.vm_id(), ?api_error, "API error details");
                }

                if is_vm_not_created_error(&err) {
                    info!(
                        vm_id = self.vm_id(),
                        "VM is not created yet, creating it from desired config"
                    );

                    self.create_vm(config.clone())
                        .await
                        .wrap_err(eyre!("Failed to create VM with id {}", self.vm_id()))?;

                    info!(vm_id = self.vm_id(), "Successfully applied config for VM");
                    return Ok(());
                }

                return Err(err).wrap_err(eyre!(
                    "Failed to inspect current state for VM with id {}",
                    self.vm_id()
                ));
            }
        };

        match vm_state {
            State::Shutdown => {
                self.create_vm(config.clone())
                    .await
                    .wrap_err(eyre!("Failed to create VM with id {}", self.vm_id()))?;
            }
            State::Created => {
                info!(
                    vm_id = self.vm_id(),
                    "VM is in Created state, recreating VM to reconcile"
                );
                self.delete_vm().await.wrap_err(eyre!(
                    "Failed to delete existing VM with id {} to apply new config",
                    self.vm_id()
                ))?;

                self.create_vm(config.clone()).await.wrap_err(eyre!(
                    "Failed to create VM with id {} after deleting existing VM",
                    self.vm_id()
                ))?;
            }
            State::Running | State::Paused => {
                self.shutdown_vm()
                    .await
                    .wrap_err(eyre!("Failed to shutdown VM with id {}", self.vm_id()))?;

                self.create_vm(config.clone())
                    .await
                    .wrap_err(eyre!("Failed to create VM with id {}", self.vm_id()))?;
            }
        }

        info!(vm_id = self.vm_id(), "Successfully applied config for VM");

        Ok(())
    }

    /// Call a custom API path with an optional JSON body on the socket.
    async fn call(&self, path: &str, body: Option<&Value>) -> Result<()> {
        let normalized_path = normalize_api_path(path)?;
        let method = default_method_for_path(&normalized_path, body);

        call_with_method(self.ch_socket_path(), method, &normalized_path, body)
            .await
            .wrap_err(eyre!(
                "Failed to call custom CH API path {} for VM with id {}",
                normalized_path,
                self.vm_id()
            ))
    }
}
// do some kind of API here, call home to gateway/orchestrator
// then create systemd services of `odorobo-ch@<vmid>.service` for each VM
#[derive(Debug, Clone)]
pub struct VMInstance {
    pub id: String, // ulid
    pub ch_socket_path: PathBuf,
}

impl VMInstance {
    const CONFIG_FILE_NAME: &'static str = "config.json";
    const SOCKET_FILE_NAME: &'static str = "ch.sock";
    const VMS_DIR_NAME: &'static str = "vms";

    pub fn new(id: &str, ch_socket_path: PathBuf) -> Self {
        Self {
            id: id.to_string(),
            ch_socket_path,
        }
    }

    /// Create a new instance for a VM given its ID and an optional config, optionally also applying the config if provided.
    #[tracing::instrument(skip(config))] // don't log the config since it can be big and contain sensitive info
    pub async fn create_instance(id: &str, config: Option<VmConfig>) -> Result<Self> {
        // Call whatever system orchestrator to spawn CH instance

        // For now we will just do this manually, see `systemd/odorobo-ch@.service` for the expected setup of the CH instance
        // ```sh
        // systemctl start odorobo-ch@<id>.service
        // ```

        // Now let's instantiate the VMInstance struct for this VM

        info!(vm_id = id, socket_path = ?Self::configured_runtime_root().join(Self::VMS_DIR_NAME).join(id).join(Self::SOCKET_FILE_NAME), "Creating VMInstance for new VM");
        let instance = Self::new(
            id,
            Self::configured_runtime_root()
                .join(Self::VMS_DIR_NAME)
                .join(id)
                .join(Self::SOCKET_FILE_NAME),
        );

        const MAX_ATTEMPTS: u32 = 31;
        let mut socket_available = false;
        for attempt in 0..MAX_ATTEMPTS {
            trace!(
                vm_id = id,
                attempt, "Pinging CH socket waiting to become available..."
            );
            if instance.vmm_ping_get().await.is_ok() {
                trace!(vm_id = id, "Socket available, VMM is up!");
                socket_available = true;
                break;
            } else {
                trace!(vm_id = id, attempt, "Socket not available yet, retrying...");
            }
            if attempt < MAX_ATTEMPTS - 1 {
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            }
        }
        if !socket_available {
            return Err(eyre!(
                "Failed to ping CH socket for VM with id {id} after 31 attempts, did the process orchestrator start the CH instance correctly?"
            ));
        }
        Ok(instance)
    }

    /// Destroy the CH instance for this VM.
    /// This should be called when the VM is shut down and we no longer need to keep runtime state around, freeing up
    /// host resources.
    ///
    /// Remember to also ask the process orchestrator to stop the CH instance when we're done!
    pub async fn destroy_instance(&self) -> Result<()> {
        // ask CH to shut down the VM if it's still running, we want to do this gracefully if possible to avoid data loss
        if let Ok(info) = self.info().await {
            if info.state == State::Running || info.state == State::Paused {
                info!(
                    vm_id = self.vm_id(),
                    "Shutting down running VM before destroying instance"
                );
                self.shutdown_vm().await.wrap_err(eyre!(
                    "Failed to shutdown VM with id {} before destroying instance",
                    self.vm_id()
                ))?;
            }
        }

        // now ask CH to shut itself down
        self.shutdown_vmm().await.wrap_err(eyre!(
            "Failed to shutdown CH VMM instance for VM with id {}",
            self.vm_id()
        ))?;

        // Call whatever system orchestrator to destroy CH instance and clean up the deployment

        // For now we will just do this manually, see `systemd/odorobo-ch@.service` for the expected setup of the CH instance
        // ```sh
        // systemctl stop odorobo-ch@<id>.service
        // ```

        // Now purge runtime state for this VM since we no longer need it
        let purge_result = self.purge_instance_data();
        if let Err(err) = purge_result {
            warn!(
                vm_id = self.vm_id(),
                ?err,
                "Failed to purge runtime data for VM with id {}, manual cleanup may be required",
                self.vm_id()
            );
        }

        Ok(())
    }

    pub fn purge_instance_data(&self) -> Result<()> {
        let runtime_dir = self.runtime_dir();
        if runtime_dir.exists() {
            fs::remove_dir_all(runtime_dir).wrap_err(eyre!(
                "Failed to remove runtime directory for VM with id {}",
                self.vm_id()
            ))?;
        }
        Ok(())
    }

    pub fn configured_runtime_root() -> PathBuf {
        env::var_os(RUNTIME_ROOT_ENV_VAR)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_RUNTIME_ROOT_DIR))
    }

    /// Return the path to the runtime root directory for the agent, which is where the agent stores runtime state for VMs.
    pub fn runtime_root() -> PathBuf {
        Self::configured_runtime_root().join(Self::VMS_DIR_NAME)
    }

    /// Return the path to the runtime directory for a given VM instance ID, which is where the agent stores runtime state for that VM instance.
    pub fn runtime_dir_for(id: &str) -> PathBuf {
        Self::runtime_root().join(id)
    }

    /// Return the path to the runtime directory for this VM instance, which is where the agent stores runtime state for this VM instance.
    pub fn runtime_dir(&self) -> PathBuf {
        Self::runtime_dir_for(&self.id)
    }

    /// Return the path to the CH API socket for this VM instance, if it exists.
    pub fn conn(&self) -> SocketBasedApiClient {
        cloud_hypervisor_client::socket_based_api_client(self.ch_socket_path.clone())
    }

    /// List running VM instances
    ///
    /// Looks up subdirectories in the runtime root directory, checking if there's a running CH
    /// socket in each subdirectory, and returns a list of VMInstance for each valid one it finds.
    pub fn list() -> Result<Vec<Self>> {
        Ok(fs::read_dir(Self::runtime_root())?
            .filter_map(|entry| {
                entry.ok().and_then(|entry| {
                    if !entry.file_type().ok()?.is_dir() {
                        return None;
                    }

                    let id = entry.file_name().to_string_lossy().to_string();
                    let ch_socket_path = Self::runtime_dir_for(&id).join(Self::SOCKET_FILE_NAME);

                    Some(Self::new(&id, ch_socket_path))
                })
            })
            .collect())
    }
}

macro_rules! delegate_default_api_no_args {
    ($name:ident -> $output:ty) => {
        fn $name(
            &self,
        ) -> Pin<Box<dyn Future<Output = std::result::Result<$output, ChApiClientError>> + Send>> {
            let client = self.conn();
            Box::pin(async move { client.$name().await })
        }
    };
}

macro_rules! delegate_default_api_one_arg {
    ($name:ident, $arg:ident : $arg_ty:ty, $output:ty) => {
        fn $name(
            &self,
            $arg: $arg_ty,
        ) -> Pin<Box<dyn Future<Output = std::result::Result<$output, ChApiClientError>> + Send>> {
            let client = self.conn();
            Box::pin(async move { client.$name($arg).await })
        }
    };
}

// Delegate API methods to the helper
impl DefaultApi for VMInstance {
    delegate_default_api_no_args!(boot_vm -> ());
    delegate_default_api_one_arg!(create_vm, vm_config: models::VmConfig, ());
    delegate_default_api_no_args!(delete_vm -> ());
    delegate_default_api_no_args!(pause_vm -> ());
    delegate_default_api_no_args!(power_button_vm -> ());
    delegate_default_api_no_args!(reboot_vm -> ());
    delegate_default_api_no_args!(resume_vm -> ());
    delegate_default_api_no_args!(shutdown_vm -> ());
    delegate_default_api_no_args!(shutdown_vmm -> ());
    delegate_default_api_one_arg!(vm_add_device_put, device_config: models::DeviceConfig, models::PciDeviceInfo);
    delegate_default_api_one_arg!(vm_add_disk_put, disk_config: models::DiskConfig, models::PciDeviceInfo);
    delegate_default_api_one_arg!(vm_add_fs_put, fs_config: models::FsConfig, models::PciDeviceInfo);
    delegate_default_api_one_arg!(vm_add_net_put, net_config: models::NetConfig, models::PciDeviceInfo);
    delegate_default_api_one_arg!(vm_add_pmem_put, pmem_config: models::PmemConfig, models::PciDeviceInfo);
    delegate_default_api_one_arg!(vm_add_user_device_put, vm_add_user_device: models::VmAddUserDevice, models::PciDeviceInfo);
    delegate_default_api_one_arg!(vm_add_vdpa_put, vdpa_config: models::VdpaConfig, models::PciDeviceInfo);
    delegate_default_api_one_arg!(vm_add_vsock_put, vsock_config: models::VsockConfig, models::PciDeviceInfo);
    delegate_default_api_one_arg!(vm_coredump_put, vm_coredump_data: models::VmCoredumpData, ());
    delegate_default_api_no_args!(vm_counters_get -> std::collections::HashMap<String, std::collections::HashMap<String, i64>>);
    delegate_default_api_no_args!(vm_info_get -> models::VmInfo);
    delegate_default_api_one_arg!(vm_receive_migration_put, receive_migration_data: models::ReceiveMigrationData, ());
    delegate_default_api_one_arg!(vm_remove_device_put, vm_remove_device: models::VmRemoveDevice, ());
    delegate_default_api_one_arg!(vm_resize_put, vm_resize: models::VmResize, ());
    delegate_default_api_one_arg!(vm_resize_zone_put, vm_resize_zone: models::VmResizeZone, ());
    delegate_default_api_one_arg!(vm_restore_put, restore_config: models::RestoreConfig, ());
    delegate_default_api_one_arg!(vm_send_migration_put, send_migration_data: models::SendMigrationData, ());
    delegate_default_api_one_arg!(vm_snapshot_put, vm_snapshot_config: models::VmSnapshotConfig, ());
    delegate_default_api_no_args!(vmm_nmi_put -> ());
    delegate_default_api_no_args!(vmm_ping_get -> models::VmmPingResponse);
}

impl VMStateManager for VMInstance {
    fn vm_id(&self) -> &str {
        &self.id
    }

    fn ch_socket_path(&self) -> &Path {
        &self.ch_socket_path
    }

    fn config_path(&self) -> PathBuf {
        self.runtime_dir().join(Self::CONFIG_FILE_NAME)
    }
}

fn find_api_error(err: &stable_eyre::Report) -> Option<&ApiError> {
    err.chain().find_map(|cause| {
        cause
            .downcast_ref::<ChApiClientError>()
            .and_then(|api_error| match api_error {
                ChApiClientError::Api(api_error) => Some(api_error),
                _ => None,
            })
    })
}

fn is_vm_not_created_error(err: &stable_eyre::Report) -> bool {
    let Some(api_error) = find_api_error(err) else {
        return false;
    };

    if api_error.body.contains("VM is not created") {
        return true;
    }

    serde_json::from_str::<Vec<String>>(&api_error.body)
        .map(|messages| {
            messages
                .iter()
                .any(|message| message == "VM is not created")
        })
        .unwrap_or(false)
}

async fn call_with_method(
    socket_path: &Path,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<()> {
    let api_path = format!("/api/v1{path}");
    let uri: hyper::Uri = hyperlocal::Uri::new(socket_path, &api_path).into();
    let client = hyper_util::client::legacy::Client::unix();
    let request_body = body.map(Value::to_string).unwrap_or_default();

    let mut request = Request::builder().method(method).uri(uri);

    if body.is_some() {
        request = request.header(CONTENT_TYPE, "application/json");
    }

    let request = request
        .body(request_body)
        .wrap_err("Failed to build CH API request")?;

    let response = client
        .request(request)
        .await
        .wrap_err("Failed to send CH API request over unix socket")?;

    let status = response.status();
    let response_body = response
        .into_body()
        .collect()
        .await
        .wrap_err("Failed to read CH API response body")?
        .to_bytes();

    if !status.is_success() {
        let response_body = String::from_utf8_lossy(&response_body);
        return Err(eyre!(
            "CH API returned {} for {}: {}",
            status,
            path,
            response_body
        ));
    }

    Ok(())
}

fn normalize_api_path(path: &str) -> Result<String> {
    let path = path.trim();

    if path.is_empty() {
        return Err(eyre!("CH API path cannot be empty"));
    }

    let path = path
        .strip_prefix("/api/v1")
        .or_else(|| path.strip_prefix("api/v1"))
        .unwrap_or(path)
        .trim_start_matches('/');

    if path.is_empty() {
        return Err(eyre!("CH API path cannot point to the API root only"));
    }

    Ok(format!("/{path}"))
}

fn default_method_for_path(path: &str, body: Option<&Value>) -> Method {
    if body.is_some() {
        return Method::PUT;
    }

    if path.ends_with(".info") || path.ends_with(".counters") || path.ends_with(".ping") {
        Method::GET
    } else {
        Method::PUT
    }
}
#[tracing::instrument]
pub fn init() -> Result<()> {
    info!("Initializing state manager");
    if !VMInstance::runtime_root().exists() {
        fs::create_dir_all(VMInstance::runtime_root())?;
    }
    Ok(())
}
