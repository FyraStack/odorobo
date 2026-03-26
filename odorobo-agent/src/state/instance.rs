use cloud_hypervisor_client::{
    SocketBasedApiClient,
    apis::DefaultApi,
    models::{self, VmInfo},
};
use hyper::Method;
use serde_json::Value;
use stable_eyre::{
    Result,
    eyre::{Context, eyre},
};
use std::{
    env, fs,
    path::{Path, PathBuf},
};
use tracing::{debug, info, warn};

use super::api::call;
use super::transform::apply_builtin_transforms;

pub const CONFIG_FILE_NAME: &str = "config.json";
const SOCKET_FILE_NAME: &str = "ch.sock";
pub const VMS_DIR_NAME: &str = "vms";

const DEFAULT_RUNTIME_ROOT_DIR: &str = "/run/odorobo";
const RUNTIME_ROOT_ENV_VAR: &str = "ODOROBO_RUNTIME_DIR";

#[derive(Debug, Clone)]
pub struct VMInstance {
    pub id: String,
    pub ch_socket_path: PathBuf,
}

impl VMInstance {
    pub fn new(id: &str, ch_socket_path: PathBuf) -> Self {
        Self {
            id: id.to_string(),
            ch_socket_path,
        }
    }

    pub fn runtime_root() -> PathBuf {
        Self::configured_runtime_root().join(VMS_DIR_NAME)
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

    pub fn ch_socket_path(&self) -> &Path {
        &self.ch_socket_path
    }

    async fn info(&self) -> Result<VmInfo> {
        self.conn()
            .vm_info_get()
            .await
            .wrap_err(eyre!("Failed to get VM info for {}", self.vm_id()))
    }

    async fn shutdown(&self) -> Result<()> {
        self.conn()
            .shutdown_vm()
            .await
            .wrap_err(eyre!("Failed to shutdown VM {}", self.vm_id()))
    }

    pub async fn console_socket(&self) -> Result<PathBuf> {
        let vminfo = self.info().await?;
        if let Some(console) = vminfo.config.console {
            match console.mode {
                models::console_config::Mode::Pty => {
                    debug!(
                        vm_id = self.vm_id(),
                        "VM has PTY console configured, returning socket path"
                    );
                    let socket_path = console.file.ok_or_else(|| {
                        eyre!("Console config is missing file path for PTY console")
                    })?;
                    Ok(socket_path.into())
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
    /// The caller should handle config persistence and VM creation.
    pub async fn spawn(id: &str) -> Result<Self> {
        info!(
            vm_id = id,
            socket_path = ?Self::runtime_dir_for(id).join(SOCKET_FILE_NAME),
            "Spawning CH process for new VM"
        );

        let instance = Self::new(id, Self::runtime_dir_for(id).join(SOCKET_FILE_NAME));

        const MAX_ATTEMPTS: u32 = 31;
        for attempt in 0..MAX_ATTEMPTS {
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
    ///
    /// Does NOT stop the CH deployment - that should be handled by the orchestrator.
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

        self.conn()
            .shutdown_vmm()
            .await
            .wrap_err(eyre!("Failed to shutdown VMM for {}", self.vm_id()))?;

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
    /// 2. Boots the VM
    pub async fn create_and_boot(&self, config: models::VmConfig) -> Result<()> {
        let mut config = config;
        apply_builtin_transforms(&mut config).wrap_err("Failed to apply config transforms")?;

        self.save_config(&config)?;
        self.conn()
            .create_vm(config)
            .await
            .wrap_err(eyre!("Failed to create VM {}", self.vm_id()))?;

        self.conn()
            .boot_vm()
            .await
            .wrap_err(eyre!("Failed to boot VM {}", self.vm_id()))?;

        info!(vm_id = self.vm_id(), "VM created and booted");
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

    /// List running VM instances.
    ///
    /// Scans runtime root for directories with valid sockets.
    pub fn list() -> Result<Vec<Self>> {
        Ok(fs::read_dir(Self::runtime_root())?
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
