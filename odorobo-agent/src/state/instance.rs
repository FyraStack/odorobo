use cloud_hypervisor_client::{
    SocketBasedApiClient,
    apis::{DefaultApi, Error as ChClientError},
    models::{self, VmInfo, VmmPingResponse},
};
use hyper::Method;
use hyper::{Request, Response, body::Bytes};
use serde_json::Value;
use stable_eyre::{
    Result,
    eyre::{Context, eyre},
};
use std::{
    env, fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tracing::{debug, info, trace, warn};

use super::api::{call, call_request};
use super::transform::apply_builtin_transforms;

pub const CONFIG_FILE_NAME: &str = "config.json";
const SOCKET_FILE_NAME: &str = "ch.sock";
pub const VMS_DIR_NAME: &str = "vms";
pub type ConsoleStream = tokio::fs::File;

const DEFAULT_RUNTIME_ROOT_DIR: &str = "/run/odorobo";
const RUNTIME_ROOT_ENV_VAR: &str = "ODOROBO_RUNTIME_DIR";

#[derive(Debug, Clone)]
pub struct VMInstance {
    pub id: String,
    pub ch_socket_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum ChApiError {
    #[error("Cloud Hypervisor API error {status}: {errors:?}")]
    Api {
        status: hyper::StatusCode,
        errors: Vec<String>,
    },
    #[error(transparent)]
    Client(ChClientError),
}

impl From<ChClientError> for ChApiError {
    fn from(error: ChClientError) -> Self {
        match error {
            ChClientError::Api(api) => Self::Api {
                status: api.code,
                errors: serde_json::from_str::<Vec<String>>(&api.body)
                    .unwrap_or_else(|_| vec![api.body]),
            },
            other => Self::Client(other),
        }
    }
}

impl VMInstance {
    pub fn new(id: &str, ch_socket_path: PathBuf) -> Self {
        Self {
            id: id.to_string(),
            ch_socket_path,
        }
    }

    pub fn get(vmid: &str) -> Option<Self> {
        Self::list().ok()?.into_iter().find(|i| i.id == vmid)
    }

    pub async fn boot(&self) -> Result<()> {
        self.conn()
            .boot_vm()
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to boot VM {}", self.vm_id()))
    }

    pub async fn pause(&self) -> Result<()> {
        self.conn()
            .pause_vm()
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to pause VM {}", self.vm_id()))
    }

    pub async fn resume(&self) -> Result<()> {
        self.conn()
            .resume_vm()
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to resume VM {}", self.vm_id()))
    }

    #[tracing::instrument]
    pub async fn send_migration(&self, dest: &str) -> Result<()> {
        let conn = self.conn();
        trace!(destination = dest, "Sending migration command to VM");

        let send_migration_data = models::SendMigrationData {
            destination_url: dest.to_string(),
            ..Default::default()
        };

        conn.vm_send_migration_put(send_migration_data)
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!(
                "Failed to send migration command for {}",
                self.vm_id()
            ))
    }

    #[tracing::instrument]
    pub async fn prepare_migration(&self) -> Result<()> {
        let conn = self.conn();
        trace!("Preparing VM for migration");

        let rand_port = random_port::PortPicker::new().pick()?;

        trace!(port = rand_port, "Selected random port for migration");

        let reciever_uri = format!("tcp:0.0.0.0:{}", rand_port);

        let receive_migration_data = models::ReceiveMigrationData {
            receiver_url: reciever_uri.clone(),
            ..Default::default()
        };

        conn.vm_receive_migration_put(receive_migration_data)
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to prepare VM for migration {}", self.vm_id()))?;

        info!(
            vm_id = self.vm_id(),
            uri = ?reciever_uri,
            "VM prepared for migration, receiver listening"
        );

        Ok(())
    }

    pub fn runtime_root() -> PathBuf {
        Self::configured_runtime_root().join(VMS_DIR_NAME)
    }

    pub fn validate_vmid(vmid: &str) -> Result<()> {
        if vmid.is_empty() {
            return Err(eyre!("VM ID cannot be empty"));
        }
        if vmid.contains("..") {
            return Err(eyre!("VM ID cannot contain path traversal sequences"));
        }
        if vmid.contains('/') || vmid.contains('\\') {
            return Err(eyre!("VM ID cannot contain path separators"));
        }
        if vmid.starts_with('.') {
            return Err(eyre!("VM ID cannot start with a dot"));
        }
        Ok(())
    }

    pub fn runtime_dir_for(id: &str) -> PathBuf {
        Self::runtime_root().join(id)
    }

    pub fn runtime_dir(&self) -> PathBuf {
        Self::runtime_dir_for(&self.id)
    }

    pub fn config_path(&self) -> PathBuf {
        self.runtime_dir().join(CONFIG_FILE_NAME)
    }

    pub fn configured_runtime_root() -> PathBuf {
        env::var_os(RUNTIME_ROOT_ENV_VAR)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_RUNTIME_ROOT_DIR))
    }

    fn conn(&self) -> SocketBasedApiClient {
        cloud_hypervisor_client::socket_based_api_client(self.ch_socket_path.clone())
    }

    pub fn vm_id(&self) -> &str {
        &self.id
    }

    /// Opens the PTY console device for this VM and returns a connected stream.
    pub async fn open_console(&self) -> Result<ConsoleStream> {
        let console_path = self.console_path().await?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&console_path)
            .wrap_err_with(|| {
                eyre!(
                    "Failed to open PTY console device for {} at {}",
                    self.vm_id(),
                    console_path.display()
                )
            })?;
        Ok(tokio::fs::File::from_std(file))
    }

    pub fn ch_socket_path(&self) -> &Path {
        &self.ch_socket_path
    }

    pub async fn info(&self) -> Result<VmInfo> {
        self.conn()
            .vm_info_get()
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to get VM info for {}", self.vm_id()))
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.conn()
            .shutdown_vm()
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to shutdown VM {}", self.vm_id()))
    }

    pub async fn acpi_power_button(&self) -> Result<()> {
        self.conn()
            .power_button_vm()
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!(
                "Failed to send ACPI power button event to VM {}",
                self.vm_id()
            ))
    }

    pub async fn ping(&self) -> Result<VmmPingResponse> {
        self.conn()
            .vmm_ping_get()
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to ping VM {}", self.vm_id()))
    }

    pub async fn console_path(&self) -> Result<PathBuf> {
        let vminfo = self.info().await?;
        if let Some(console) = vminfo.config.console {
            match console.mode {
                models::console_config::Mode::Pty => {
                    debug!(
                        vm_id = self.vm_id(),
                        "VM has PTY console configured, returning PTY path"
                    );
                    let console_path = console.file.ok_or_else(|| {
                        eyre!("Console config is missing file path for PTY console")
                    })?;
                    Ok(console_path.into())
                }
                _ => Err(eyre!(
                    "Console is configured but is not a PTY console, unsupported console type"
                )),
            }
        } else {
            Err(eyre!("VM does not have a console configured nor supported"))
        }
    }

    /// Spawn a new CH process and create a VMInstance for it.
    ///
    /// Waits for the socket to become available (polls up to ~30 seconds).
    /// Calls a backend to handle the actual CH process spawning - typically a systemd unit
    pub async fn spawn(id: &str) -> Result<Self> {
        info!(
            vm_id = id,
            socket_path = ?Self::runtime_dir_for(id).join(SOCKET_FILE_NAME),
            "Spawning CH process for new VM"
        );

        let provisioner = super::provisioning::default_provisioner();
        provisioner.start_instance(id).await?;

        let instance = Self::new(id, Self::runtime_dir_for(id).join(SOCKET_FILE_NAME));

        const MAX_ATTEMPTS: u32 = 31;
        for attempt in 0..MAX_ATTEMPTS {
            trace!(vm_id = id, attempt, "Checking if CH socket is available");
            if instance.conn().vmm_ping_get().await.is_ok() {
                debug!(vm_id = id, "CH socket available");
                return Ok(instance);
            }

            if attempt < MAX_ATTEMPTS - 1 {
                tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
            }
        }

        Err(eyre!(
            "CH socket not available after {} attempts for VM {}",
            MAX_ATTEMPTS,
            id
        ))
    }

    /// Gracefully shutdown the VM and VMM, then clean up runtime state.
    pub async fn destroy(&self) -> Result<()> {
        if let Ok(info) = self.info().await {
            if matches!(
                info.state,
                models::vm_info::State::Running | models::vm_info::State::Paused
            ) {
                info!(vm_id = self.vm_id(), "Shutting down VM before destroy");
                self.shutdown().await?;
            }
        }
        let provisioner = super::provisioning::default_provisioner();

        self.conn()
            .shutdown_vmm()
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to shutdown VMM for {}", self.vm_id()))?;

        provisioner.stop_instance(self.vm_id()).await?;

        if let Err(err) = self.purge_instance_data() {
            warn!(
                vm_id = self.vm_id(),
                ?err,
                "Failed to purge runtime data, manual cleanup may be required"
            );
        }

        Ok(())
    }

    /// Purge the runtime data for this VM instance.
    ///
    /// This removes the runtime directory and all its contents if it exists.
    pub fn purge_instance_data(&self) -> Result<()> {
        let runtime_dir = self.runtime_dir();
        if runtime_dir.exists() {
            fs::remove_dir_all(runtime_dir).wrap_err(eyre!(
                "Failed to remove runtime directory for {}",
                self.vm_id()
            ))?;
        }
        Ok(())
    }

    /// Load desired VM config from disk.
    pub fn load_config(&self) -> Result<models::VmConfig> {
        let config_data = fs::read_to_string(self.config_path())
            .wrap_err(eyre!("Failed to read config file for {}", self.vm_id()))?;

        serde_json::from_str(&config_data)
            .wrap_err(eyre!("Failed to parse config JSON for {}", self.vm_id()))
    }

    /// Save desired VM config to disk.
    pub fn save_config(&self, config: &models::VmConfig) -> Result<()> {
        let config_data =
            serde_json::to_string_pretty(config).wrap_err("Failed to serialize config to JSON")?;

        fs::write(self.config_path(), config_data)
            .wrap_err(eyre!("Failed to write config file for {}", self.vm_id()))
    }

    /// Create and boot a VM with the given config.
    ///
    /// Applies node-specific transforms, saves config to disk, then:
    /// 1. Creates the VM via CH API
    /// 2. Boots the VM (if boot is true)
    pub async fn create(&self, config: models::VmConfig, boot: bool) -> Result<()> {
        trace!(vm_id = self.vm_id(), "Creating VM with provided config");
        let mut config = config;
        trace!(vm_id = self.vm_id(), "Applying config transforms");
        apply_builtin_transforms(&mut config).wrap_err("Failed to apply config transforms")?;

        trace!(vm_id = self.vm_id(), "Saving config to runtime dir");
        self.save_config(&config)?;

        trace!(vm_id = self.vm_id(), "Creating VM via CH API");
        self.conn()
            .create_vm(config)
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to create VM {}", self.vm_id()))?;

        if boot {
            debug!(vm_id = self.vm_id(), "Booting VM");
            self.conn()
                .boot_vm()
                .await
                .map_err(ChApiError::from)
                .wrap_err(eyre!("Failed to boot VM {}", self.vm_id()))?;
        }
        info!(vm_id = self.vm_id(), "VM created and booted");
        Ok(())
    }

    pub async fn delete_config(&self) -> Result<()> {
        let conn = self.conn();
        trace!(vm_id = self.vm_id(), "Deleting VM via CH API");
        conn.delete_vm()
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to delete VM {}", self.vm_id()))?;
        let config_path = self.config_path();
        if config_path.exists() {
            fs::remove_file(config_path)
                .wrap_err(eyre!("Failed to remove config file for {}", self.vm_id()))?;
        }
        Ok(())
    }

    /// Call a custom CH API path with an explicit HTTP method.
    ///
    /// For debugging: lets you hit any CH API endpoint directly.
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<()> {
        call(self.ch_socket_path(), method, path, body)
            .await
            .wrap_err(eyre!("Failed to call {} for {}", path, self.vm_id()))
    }

    /// Proxy a raw HTTP request to the CH API socket.
    pub async fn call_request(&self, request: Request<Bytes>) -> Result<Response<Bytes>> {
        call_request(self.ch_socket_path(), request)
            .await
            .wrap_err(eyre!("Failed to proxy CH API request for {}", self.vm_id()))
    }

    /// List running VM instances.
    ///
    /// Scans runtime root for directories with valid sockets.
    pub fn list() -> Result<Vec<Self>> {
        let root = Self::runtime_root();
        fs::create_dir_all(&root)?;
        Ok(fs::read_dir(root)?
            .filter_map(|entry| {
                entry.ok().and_then(|entry| {
                    if !entry.file_type().ok()?.is_dir() {
                        return None;
                    }

                    let id = entry.file_name().to_string_lossy().to_string();
                    let ch_socket_path = Self::runtime_dir_for(&id).join(SOCKET_FILE_NAME);

                    Some(Self::new(&id, ch_socket_path))
                })
            })
            .collect())
    }
}
