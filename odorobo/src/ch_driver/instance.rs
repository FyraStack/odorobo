use cloud_hypervisor_client::{
    SocketBasedApiClient,
    apis::{DefaultApi, Error as ChClientError},
    models::{self, VmConfig, VmInfo, VmmPingResponse},
};
use hyper::{Request, Response, body::Bytes};
use stable_eyre::{
    Result,
    eyre::{Context, eyre},
};
use std::{
    env,
    fs::{self, File},
    io::BufWriter,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tokio::task::JoinHandle;
use tracing::{debug, info, trace, warn};

use crate::ch_driver::{
    provisioning::hooks::HookManager,
    transform::{ConfigTransform, TransformChain},
};

use super::api::call_request;

pub const CONFIG_FILE_NAME: &str = "config.json";
const SOCKET_FILE_NAME: &str = "ch.sock";
pub const VMS_DIR_NAME: &str = "vms";
pub type ConsoleStream = std::fs::File;

const DEFAULT_RUNTIME_ROOT_DIR: &str = "/run/odorobo";
const RUNTIME_ROOT_ENV_VAR: &str = "ODOROBO_RUNTIME_DIR";

pub struct VMInstance {
    pub id: String,
    pub ch_socket_path: PathBuf,
    transformer: TransformChain,
    hook_manager: HookManager,
    child_process: Option<tokio::process::Child>,
    /// Pre-transformed VM config, if available
    pub vm_config: Option<models::VmConfig>,
}

impl std::fmt::Debug for VMInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VMInstance")
            .field("id", &self.id)
            .field("ch_socket_path", &self.ch_socket_path)
            .field("vm_config", &self.vm_config)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
#[error("failed to confirm VMM process exit after startup failure: {0}")]
pub struct UnconfirmedProcessExit(std::io::Error);

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
    fn new(
        id: &str,
        ch_socket_path: PathBuf,
        transformer: Option<TransformChain>,
        child_process: Option<tokio::process::Child>,
    ) -> Self {
        Self {
            id: id.to_owned(),
            ch_socket_path,
            transformer: transformer.unwrap_or_default(),
            hook_manager: HookManager::default(),
            child_process,
            vm_config: None,
        }
    }

    /// Takes the child process out of this instance, transferring ownership to the caller.
    /// Useful for watching the process lifecycle externally (e.g. in an actor watcher task).
    /// After calling this, `destroy()` will skip the child-kill step.
    pub const fn take_child_process(&mut self) -> Option<tokio::process::Child> {
        self.child_process.take()
    }

    /// Get a VM instance by its ID through the filesystem database
    ///
    /// Not reliable as of 0.2
    #[deprecated(since = "0.2.0")]
    pub fn get(vmid: &str) -> Option<Self> {
        #[expect(
            deprecated,
            reason = "deprecated filesystem getter is implemented in terms of the deprecated filesystem listing"
        )]
        let instances = Self::list().ok()?;

        instances.into_iter().find(|i| i.id == vmid)
    }

    pub async fn boot(&self) -> Result<()> {
        // boot hooks

        let vm_config = self.info().await?.config;

        debug!(vmid = self.vm_id(), "before_boot hooks invoked");
        self.hook_manager
            .before_boot(self.vm_id(), &vm_config)
            .await?;

        debug!(vmid = self.vm_id(), "boot_vm invoked");
        self.conn()
            .boot_vm()
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to boot VM {}", self.vm_id()))?;

        // get VM info after boot

        let vm_info_postboot = self.info().await?;

        debug!(vmid = self.vm_id(), "after_boot hooks invoked");
        self.hook_manager
            .after_boot(self.vm_id(), &vm_info_postboot)
            .await?;

        Ok(())
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

    /// Initiates a migration from this VM to the specified destination URI.
    ///
    /// Options:
    /// - `dest`: the destination URI to migrate to, in the format expected by CH (e.g. "tcp:<`IP_ADDRESS>:12345`")
    /// - `local`: if true, indicates that the migration is local (e.g. within the same host, for renaming a VM). This is passed to CH and may affect how the migration is performed.
    #[tracing::instrument]
    pub async fn send_migration(&mut self, dest: &str, local: bool) -> Result<()> {
        let conn = self.conn();
        trace!(destination = dest, "Sending migration command to VM");

        let send_migration_data = models::SendMigrationData {
            destination_url: dest.to_owned(),
            local: Some(local),
            ..Default::default()
        };

        conn.vm_send_migration_put(send_migration_data)
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!(
                "Failed to send migration command for {}",
                self.vm_id()
            ))?;

        // if migration is successful, we can assume the VM is effectively "gone" from this host, so we can clean up runtime state.
        self.purge_instance_data()
            .wrap_err("Failed to purge instance data after migration")?;

        Ok(())
    }

    /// Prepares the VM to receive a migration by starting a migration receiver in the background.
    /// Returns the URI that the sender should connect to for migration.
    ///
    /// Note: This does not currently track migration state,
    /// so it's currently up to the caller to make sure that the receiver is ready before the sender tries to connect.
    /// Future improvement: add some kind of global tracker for active migrations and their states.
    #[tracing::instrument]
    pub async fn receive_migration(&self) -> Result<(String, JoinHandle<Result<()>>)> {
        let conn = self.conn();
        trace!("Preparing VM for migration");

        let rand_port = random_port::PortPicker::new()
            .port_range(49152u16..=65535u16)
            .pick()?;

        trace!(port = rand_port, "Selected random port for migration");

        let receiver_uri = format!("tcp:0.0.0.0:{rand_port}");

        let receive_migration_data = models::ReceiveMigrationData {
            receiver_url: receiver_uri.clone(),
        };

        let vm_id = self.vm_id().to_owned();
        info!(
            vm_id,
            port = rand_port,
            "Preparing VM for migration, spawning receiver in background"
        );

        let migration_task = tokio::spawn(async move {
            conn.vm_receive_migration_put(receive_migration_data)
                .await
                .map_err(ChApiError::from)
                .wrap_err(eyre!("Failed to prepare VM for migration {}", vm_id))?;
            info!(vm_id, "Migration receiver completed successfully");
            // This is the destination VMM: it now owns the running guest and
            // must remain alive after a successful receive.
            Ok(())
        });

        Ok((receiver_uri, migration_task))
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
            .map_or_else(|| PathBuf::from(DEFAULT_RUNTIME_ROOT_DIR), PathBuf::from)
    }

    pub fn conn(&self) -> SocketBasedApiClient {
        cloud_hypervisor_client::socket_based_api_client(self.ch_socket_path.clone())
    }

    pub fn vm_id(&self) -> &str {
        &self.id
    }

    /// Returns the configured PTY/file path for this VM's serial console.
    #[tracing::instrument]
    pub async fn console_path(&self) -> Result<PathBuf> {
        trace!("Getting console file path from CH API");
        let serial = self
            .info()
            .await?
            .config
            .serial
            .ok_or_else(|| eyre!("No serial console configured for {}", self.vm_id()))?;
        let path = serial.file.ok_or_else(|| {
            eyre!(
                "Serial console is not configured as a file for {}",
                self.vm_id()
            )
        })?;
        Ok(PathBuf::from(path))
    }

    /// Returns the configured UNIX socket path for this VM's serial console.
    pub fn console_socket_path(&self) -> PathBuf {
        self.runtime_dir().join("console.sock")
    }

    /// Opens the PTY console device for this VM and returns a connected stream.
    #[tracing::instrument]
    pub async fn open_console(&self) -> Result<ConsoleStream> {
        let pty_path = self.console_path().await?;
        trace!(pty_path = ?pty_path, "Opening console PTY device");
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&pty_path)
            .wrap_err_with(|| {
                eyre!(
                    "Failed to open console PTY for {} at {}",
                    self.vm_id(),
                    pty_path.display()
                )
            })
    }

    pub fn ch_socket_path(&self) -> &Path {
        &self.ch_socket_path
    }

    async fn cleanup_failed_start(&mut self) -> Result<()> {
        if let Some(mut child) = self.child_process.take() {
            _ = child.start_kill();
            child.wait().await.map_err(UnconfirmedProcessExit)?;
        }
        self.cleanup_after_stop().await
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

    /// Spawn a Cloud Hypervisor process and optionally create/boot its VM.
    ///
    /// `vm_config` is already translated by the CH manifest boundary. The
    /// separate `boot` flag preserves the manifest's desired start behavior;
    /// callers can create a stopped VM without changing its provider config.
    /// The socket is polled for up to roughly 30 seconds before failing.
    #[tracing::instrument(skip_all)]
    pub async fn spawn(
        id: &str,
        vm_config: Option<VmConfig>,
        boot: bool,
        transformer: Option<TransformChain>,
    ) -> Result<Self> {
        let ch_socket_path = Self::runtime_dir_for(id).join(SOCKET_FILE_NAME);
        info!(?ch_socket_path, "Spawning VM");
        // make sure socket path parent exists
        if !ch_socket_path.parent().unwrap().exists() {
            std::fs::create_dir_all(ch_socket_path.parent().unwrap())?;
        }
        let ch_process = tokio::process::Command::new("cloud-hypervisor")
            .arg("--api-socket")
            .arg(&ch_socket_path)
            .kill_on_drop(true)
            .spawn()?;
        let mut instance = Self::new(id, ch_socket_path, transformer, Some(ch_process));

        const MAX_ATTEMPTS: u32 = 31;
        for attempt in 0..MAX_ATTEMPTS {
            info!(vm_id = id, attempt, "Checking if CH socket is available");
            if matches!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    instance.conn().vmm_ping_get()
                )
                .await,
                Ok(Ok(_))
            ) {
                info!(vm_id = id, "CH socket available");
                if let Some(vm_config) = vm_config {
                    info!(boot, ?vm_config, "Creating VM config");
                    let create_result = tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        instance.create_config(vm_config, boot),
                    )
                    .await
                    .unwrap_or_else(|_| Err(eyre!("VM creation timed out")));
                    if let Err(error) = create_result {
                        if let Err(cleanup_error) = instance.cleanup_failed_start().await {
                            warn!(
                                vm_id = id,
                                ?cleanup_error,
                                "failed to clean up unsuccessful VM startup"
                            );
                            if cleanup_error
                                .downcast_ref::<UnconfirmedProcessExit>()
                                .is_some()
                            {
                                return Err(cleanup_error.wrap_err(error.to_string()));
                            }
                        }
                        return Err(error);
                    }
                }
                return Ok(instance);
            }

            if attempt < MAX_ATTEMPTS - 1 {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }

        if let Err(cleanup_error) = instance.cleanup_failed_start().await {
            warn!(
                vm_id = id,
                ?cleanup_error,
                "failed to clean up VMM socket timeout"
            );
            if cleanup_error
                .downcast_ref::<UnconfirmedProcessExit>()
                .is_some()
            {
                return Err(cleanup_error);
            }
        }
        Err(eyre!(
            "CH socket not available after {} attempts for VM {}",
            MAX_ATTEMPTS,
            id
        ))
    }

    /// Request shutdown without removing resources still used by the VMM.
    pub async fn stop(&self) -> Result<()> {
        self.stop_with_timeouts(
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(5),
        )
        .await
    }

    async fn stop_with_timeouts(
        &self,
        guest_timeout: std::time::Duration,
        vmm_timeout: std::time::Duration,
    ) -> Result<()> {
        info!(
            vm_id = self.vm_id(),
            "Destroying VM instance, shutting down VM and cleaning up runtime state"
        );
        let stop_result = tokio::time::timeout(guest_timeout, async {
            if let Ok(info) = self.info().await {
                trace!(vm_id = self.vm_id(), state = ?info.state, "Checking VM state before destroy");
                self.hook_manager.before_stop(self.vm_id(), &info).await?;
                if matches!(
                    info.state,
                    models::VmState::Running | models::VmState::Paused
                ) {
                    info!(vm_id = self.vm_id(), "Shutting down VM before destroy");
                    self.shutdown().await?;
                }
            } else {
                warn!(
                    vm_id = self.vm_id(),
                    "Failed to get VM info before destroy, proceeding with shutdown and cleanup anyway"
                );
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|_| {
            warn!(vm_id = self.vm_id(), "graceful VM shutdown timed out; forcing process exit");
            Ok(())
        });

        // Attempt VMM shutdown even if a hook or guest shutdown failed, but
        // never let an unresponsive API prevent the caller from killing it.
        if matches!(
            tokio::time::timeout(vmm_timeout, self.conn().shutdown_vmm()).await,
            Ok(Ok(()))
        ) {
            debug!(vm_id = self.vm_id(), "VMM shutdown successfully");
        } else {
            warn!(
                vm_id = self.vm_id(),
                "Failed to shutdown VMM, assuming it is already stopped or unresponsive"
            );
        }
        stop_result
    }

    /// Gracefully shutdown the VM and VMM, confirm exit, then clean up runtime state.
    pub async fn destroy(&mut self) -> Result<()> {
        let stop_result = self.stop().await;
        if let Some(mut child) = self.child_process.take() {
            trace!("VMM stopped... checking child process");
            // start_kill can fail when the process has already exited; wait is
            // authoritative and must succeed before callers can release leases.
            _ = child.start_kill();
            child.wait().await.map_err(UnconfirmedProcessExit)?;
        }

        let cleanup_result = self.cleanup_after_stop().await;
        stop_result?;
        cleanup_result
    }

    /// Remove resources only after the owner has confirmed VMM process exit.
    pub async fn cleanup_after_stop(&mut self) -> Result<()> {
        let vm_config = self.vm_config.clone().unwrap_or_default();
        let hook_result = self.hook_manager.after_stop(self.vm_id(), &vm_config).await;
        let cleanup_result = self.purge_instance_data();
        hook_result?;
        cleanup_result
    }

    /// Purge the runtime data for this VM instance.
    ///
    /// This removes the runtime directory and all its contents if it exists.
    pub fn purge_instance_data(&mut self) -> Result<()> {
        let mut vm_config = self.vm_config.take().unwrap_or_default();
        let runtime_dir = self.runtime_dir();
        let teardown_result = self.transformer.teardown(self.vm_id(), &mut vm_config);
        let cleanup_result = remove_runtime_directory(&runtime_dir, self.vm_id());
        teardown_result?;
        cleanup_result
    }

    /// Load desired VM config from disk.
    pub fn load_config(&self) -> Result<models::VmConfig> {
        serde_json::from_reader(
            File::open(self.config_path())
                .wrap_err(eyre!("Failed to read config file for {}", self.vm_id()))?,
        )
        .wrap_err(eyre!("Failed to parse config JSON for {}", self.vm_id()))
    }

    /// Save desired VM config to disk.
    pub fn save_config(&self, config: &models::VmConfig) -> Result<()> {
        let file = File::create(self.config_path())
            .wrap_err(eyre!("Failed to open config file for {}", self.vm_id()))?;
        serde_json::to_writer_pretty(BufWriter::new(file), config)
            .wrap_err(eyre!("Failed to write config file for {}", self.vm_id()))
    }

    /// Create and boot a VM with the given config.
    ///
    /// Applies node-specific transforms, saves config to disk, then:
    /// 1. Creates the VM via CH API
    /// 2. Boots the VM (if boot is true)
    pub async fn create_config(&mut self, config: models::VmConfig, boot: bool) -> Result<()> {
        trace!(vm_id = self.vm_id(), "Creating VM with provided config");

        trace!(vm_id = self.vm_id(), "Applying config transforms");
        let mut transformed_config = config;
        let transform_result = self
            .transformer
            .transform(self.vm_id(), &mut transformed_config);
        self.vm_config = Some(transformed_config.clone());
        transform_result.wrap_err(eyre!(
            "Failed to apply config transforms for VM {}",
            self.vm_id()
        ))?;

        trace!(vm_id = self.vm_id(), "Creating VM via CH API");
        self.conn()
            .create_vm(transformed_config)
            .await
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to create VM {}", self.vm_id()))?;

        if boot {
            debug!(vm_id = self.vm_id(), "Booting VM");
            self.boot()
                .await
                .wrap_err(eyre!("Failed to boot VM {}", self.vm_id()))?;
        }
        info!(vm_id = self.vm_id(), "VM created and booted");
        Ok(())
    }

    /// Dry-apply a VM config without actually setting it in Cloud Hypervisor,
    /// allowing for live migration of the VM.
    pub async fn prep_config(&mut self, mut config: models::VmConfig) -> Result<()> {
        info!(vm_id = self.vm_id(), "Preparing VM config for migration");
        // Preserve even partial transforms so teardown can release everything
        // provisioned before an error.
        let transform_result = self.transformer.transform(self.vm_id(), &mut config);
        self.vm_config = Some(config.clone());
        transform_result.wrap_err(eyre!(
            "Failed to apply config transforms for VM {}",
            self.vm_id()
        ))?;

        self.hook_manager.before_boot(self.vm_id(), &config).await?;

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

    /// Proxy a raw HTTP request to the CH API socket.
    pub async fn call_request(&self, request: Request<Bytes>) -> Result<Response<Bytes>> {
        call_request(self.ch_socket_path(), request)
            .await
            .wrap_err(eyre!("Failed to proxy CH API request for {}", self.vm_id()))
    }

    /// List running VM instances.
    ///
    /// Scans runtime root for directories with valid sockets.
    #[deprecated(since = "0.2.0")]
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

                    Some(Self::new(&id, ch_socket_path, None, None))
                })
            })
            .collect())
    }
}

fn remove_runtime_directory(runtime_dir: &Path, vmid: &str) -> Result<()> {
    if runtime_dir.exists() {
        fs::remove_dir_all(runtime_dir)
            .wrap_err(eyre!("Failed to remove runtime directory for {vmid}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{VMInstance, remove_runtime_directory};
    use crate::ch_driver::cloud_init::create_seed_image;
    use crate::ch_driver::transform::{ConfigTransform, TransformChain};
    use crate::manifest::CloudInit;
    use cloud_hypervisor_client::models::VmConfig;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::net::UnixListener;
    use ulid::Ulid;

    struct RecordTeardown(Arc<AtomicBool>);

    impl ConfigTransform for RecordTeardown {
        fn transform(&self, _vmid: &str, _config: &mut VmConfig) -> stable_eyre::Result<()> {
            Ok(())
        }

        fn teardown(&self, _vmid: &str, _config: &mut VmConfig) -> stable_eyre::Result<()> {
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn failed_start_reaps_child_and_tears_down_provisioned_resources() {
        let directory =
            std::env::temp_dir().join(format!("odorobo-failed-start-{}", Ulid::generate()));
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let teardown = Arc::new(AtomicBool::new(false));
        let transformer = TransformChain::new().add(RecordTeardown(Arc::clone(&teardown)));
        let mut instance = VMInstance::new(
            &Ulid::generate().to_string(),
            directory.join("ch.sock"),
            Some(transformer),
            Some(child),
        );
        instance.vm_config = Some(VmConfig::default());
        instance.cleanup_failed_start().await.unwrap();
        assert!(instance.child_process.is_none());
        assert!(teardown.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unresponsive_api_cannot_block_force_kill_fallback() {
        let directory =
            std::env::temp_dir().join(format!("odorobo-stop-timeout-{}", Ulid::generate()));
        std::fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("ch.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let first = listener.accept().await.unwrap().0;
            let second = listener.accept().await.unwrap().0;
            std::future::pending::<()>().await;
            drop((first, second));
        });
        let instance = VMInstance::new(
            &Ulid::generate().to_string(),
            socket,
            Some(TransformChain::new()),
            None,
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            instance.stop_with_timeouts(
                std::time::Duration::from_millis(20),
                std::time::Duration::from_millis(20),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn runtime_cleanup_removes_cloud_init_seed_artifacts() {
        let runtime_dir =
            std::env::temp_dir().join(format!("odorobo-runtime-cleanup-{}", Ulid::generate()));
        let cloud_init = CloudInit {
            user_data: Some("#cloud-config\n".to_owned()),
            meta_data: Some("instance-id: cleanup-test\n".to_owned()),
            vendor_data: None,
        };
        let seed_path = create_seed_image(&runtime_dir, &cloud_init).expect("seed image created");
        assert!(seed_path.exists());

        remove_runtime_directory(&runtime_dir, "cleanup-test").expect("runtime data removed");
        assert!(!runtime_dir.exists());
    }
}
