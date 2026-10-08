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
use tracing::{debug, error, info, trace, warn};

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

/// mountinfo escapes whitespace and backslashes with octal sequences.
fn decode_mount_path(value: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..index + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            decoded.push(
                (bytes[index + 1] - b'0') * 64 + (bytes[index + 2] - b'0') * 8 + bytes[index + 3]
                    - b'0',
            );
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    std::ffi::OsString::from_vec(decoded).into()
}

fn check_runtime_mounts(runtime: &Path, mountinfo: &str) -> Result<()> {
    for line in mountinfo.lines() {
        let mount = line
            .split_whitespace()
            .nth(4)
            .ok_or_else(|| eyre!("invalid mountinfo record"))?;
        if decode_mount_path(mount).starts_with(runtime) {
            return Err(eyre!(
                "refusing to purge runtime {} while mounts remain",
                runtime.display()
            ));
        }
    }
    Ok(())
}

fn ensure_runtime_unmounted(runtime: &Path) -> Result<()> {
    if runtime.exists() {
        check_runtime_mounts(
            &fs::canonicalize(runtime)?,
            &fs::read_to_string("/proc/self/mountinfo")?,
        )?;
    }
    Ok(())
}

pub struct VMInstance {
    pub id: String,
    pub ch_socket_path: PathBuf,
    transformer: TransformChain,
    hook_manager: HookManager,
    child_process: std::sync::Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>,
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stopped: bool,
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
    pub(super) fn new(
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
            child_process: std::sync::Arc::new(tokio::sync::Mutex::new(child_process)),
            stopping: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stopped: false,
            vm_config: None,
        }
    }

    #[cfg(test)]
    pub(super) async fn kill_hypervisor_for_test(&self) {
        self.child_process
            .lock()
            .await
            .as_mut()
            .expect("running child")
            .start_kill()
            .unwrap();
    }

    /// Lifecycle placeholder before a hypervisor has been acquired.
    pub(super) fn stopped(id: &str) -> Self {
        let mut instance = Self::new(
            id,
            Self::runtime_dir_for(id).join(SOCKET_FILE_NAME),
            None,
            None,
        );
        instance.stopped = true;
        instance
    }

    /// Watch exit without transferring child ownership away from teardown.
    pub fn watch_child(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<std::process::ExitStatus>>> + Send + 'static
    {
        let child = self.child_process.clone();
        let stopping = self.stopping.clone();
        async move {
            loop {
                {
                    let mut slot = child.lock().await;
                    if stopping.load(std::sync::atomic::Ordering::Relaxed) {
                        return Ok(None);
                    }
                    let Some(process) = slot.as_mut() else {
                        return Ok(None);
                    };
                    if let Some(status) = process.try_wait()? {
                        if stopping.load(std::sync::atomic::Ordering::Relaxed) {
                            return Ok(None);
                        }
                        return Ok(Some(status));
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }
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
    pub async fn receive_migration(&self) -> Result<(String, JoinHandle<()>)> {
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
            match conn
                .vm_receive_migration_put(receive_migration_data)
                .await
                .map_err(ChApiError::from)
                .wrap_err(eyre!("Failed to prepare VM for migration {}", vm_id))
            {
                Ok(()) => {
                    info!(vm_id, "Migration receiver completed successfully");
                    // shut down vm
                    if let Err(e) = conn.shutdown_vmm().await {
                        error!(vm_id, error = ?e, "Failed to shut down VM after migration");
                    }
                }
                Err(e) => error!(vm_id, error = ?e, "Migration receiver failed"),
            }
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

    async fn stop_child_after_failed_start(&self) {
        if let Some(mut child) = self.child_process.lock().await.take() {
            _ = child.start_kill();
            _ = child.wait().await;
        }
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
    pub async fn spawn_into(
        owner: &mut Self,
        id: &str,
        vm_config: Option<VmConfig>,
        boot: bool,
        transformer: Option<TransformChain>,
    ) -> Result<()> {
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
        *owner = Self::new(id, ch_socket_path, transformer, Some(ch_process));
        let instance = owner;

        const MAX_ATTEMPTS: u32 = 31;
        for attempt in 0..MAX_ATTEMPTS {
            info!(vm_id = id, attempt, "Checking if CH socket is available");
            if instance.conn().vmm_ping_get().await.is_ok() {
                info!(vm_id = id, "CH socket available");
                if let Some(vm_config) = vm_config {
                    info!(boot, ?vm_config, "Creating VM config");
                    if let Err(error) = instance.create_config(vm_config, boot).await {
                        return Err(error);
                    }
                }
                return Ok(());
            }

            if attempt < MAX_ATTEMPTS - 1 {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }

        Err(eyre!(
            "CH socket not available after {} attempts for VM {}",
            MAX_ATTEMPTS,
            id
        ))
    }

    /// Stop the VM/VMM without deleting runtime paths. Rootfs teardown must
    /// finish before the separate purge step may traverse those paths.
    pub async fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        info!(
            vm_id = self.vm_id(),
            "Stopping VM and VMM without purging runtime state"
        );
        if let Ok(info) = self.info().await {
            trace!(vm_id = self.vm_id(), state = ?info.state, "Checking VM state before stop");
            self.hook_manager.before_stop(self.vm_id(), &info).await?;
            if matches!(
                info.state,
                models::VmState::Running | models::VmState::Paused
            ) {
                info!(vm_id = self.vm_id(), "Shutting down VM before stop");
                self.shutdown().await?;
            }
        } else {
            warn!(
                vm_id = self.vm_id(),
                "Failed to get VM info before stop; stopping the VMM directly"
            );
        }

        // The exit watcher must not stop this actor in the middle of a
        // retryable DeleteVM request. Teardown now owns the child lifecycle.
        self.stopping
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let vmm_shutdown = self.conn().shutdown_vmm().await;
        if vmm_shutdown.is_err() && self.child_process.lock().await.is_none() {
            return Err(eyre!(
                "VMM shutdown is unconfirmed; preserving runtime and rootfs"
            ));
        }
        if vmm_shutdown.is_ok() {
            debug!(vm_id = self.vm_id(), "VMM shutdown successfully");
        } else {
            warn!(
                vm_id = self.vm_id(),
                "Failed to shutdown VMM, assuming it is already stopped or unresponsive"
            );
        }
        let vm_config = self.vm_config.clone().unwrap_or_default();
        {
            let mut slot = self.child_process.lock().await;
            if let Some(child) = slot.as_mut() {
                // Preserve the handle if killing/reaping fails, allowing retry.
                if child.try_wait()?.is_none() {
                    child.start_kill()?;
                    child.wait().await?;
                }
            }
            *slot = None;
        }

        self.hook_manager
            .after_stop(self.vm_id(), &vm_config)
            .await?;
        self.stopped = true;
        Ok(())
    }

    /// Purge the runtime data for this VM instance.
    ///
    /// This removes the runtime directory and all its contents if it exists.
    pub fn purge_instance_data(&mut self) -> Result<()> {
        ensure_runtime_unmounted(&self.runtime_dir())?;
        let mut vm_config = self.vm_config.clone().unwrap_or_default();
        let release = self.transformer.teardown(self.vm_id(), &mut vm_config);
        self.vm_config = Some(vm_config);
        release?;
        self.vm_config = None;
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
        let transform = self
            .transformer
            .transform(self.vm_id(), &mut transformed_config);
        // Preserve partial acquisition even when a subsequent transform fails.
        self.vm_config = Some(transformed_config.clone());
        transform.wrap_err(eyre!(
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
    pub async fn prep_config(&mut self, config: models::VmConfig) -> Result<()> {
        self.vm_config = Some(config.clone());

        info!(vm_id = self.vm_id(), "Preparing VM config for migration");
        // simply "transform" the config here without actually setting it in CH, the migrator will do that for us

        let mut transformed_config = config.clone();
        self.transformer
            .transform(self.vm_id(), &mut transformed_config)
            .wrap_err(eyre!(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_purge_checks_nested_mounts_and_escaped_paths() {
        let runtime = Path::new("/run/odorobo/vms/a b");
        assert!(
            check_runtime_mounts(
                runtime,
                "1 0 0:1 / /run/odorobo/vms/a\\040b/rootfs rw - tmpfs tmpfs rw"
            )
            .is_err()
        );
        assert!(
            check_runtime_mounts(
                runtime,
                "1 0 0:1 / /run/odorobo/vms/a\\040b-other rw - tmpfs tmpfs rw"
            )
            .is_ok()
        );
        assert!(check_runtime_mounts(runtime, "malformed").is_err());
        assert_eq!(decode_mount_path("/a\\134b"), Path::new("/a\\b"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn partial_transform_failure_retains_release_inventory_for_retry() {
        use crate::ch_driver::transform::ConfigTransform;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct RecordingTransform(Arc<AtomicUsize>);
        impl ConfigTransform for RecordingTransform {
            fn transform(&self, _vmid: &str, config: &mut VmConfig) -> Result<()> {
                config.disks = Some(vec![models::DiskConfig {
                    path: Some("/dev/recorded".into()),
                    id: Some("recording://volume".into()),
                    ..Default::default()
                }]);
                Err(eyre!("later transform failed"))
            }
            fn teardown(&self, _vmid: &str, config: &mut VmConfig) -> Result<()> {
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(eyre!("release failed"));
                }
                config.disks = None;
                Ok(())
            }
        }
        let releases = Arc::new(AtomicUsize::new(0));
        let mut instance = VMInstance::new(
            "transform-fixture",
            PathBuf::from("/unused"),
            Some(TransformChain::new().add(RecordingTransform(releases.clone()))),
            None,
        );
        assert!(
            instance
                .create_config(VmConfig::default(), false)
                .await
                .is_err()
        );
        assert_eq!(
            instance.vm_config.as_ref().unwrap().disks.as_ref().unwrap()[0]
                .id
                .as_deref(),
            Some("recording://volume")
        );
        assert!(instance.purge_instance_data().is_err());
        assert!(instance.vm_config.as_ref().unwrap().disks.is_some());
        instance.purge_instance_data().unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn child_watcher_leaves_owned_child_for_teardown() {
        let child = tokio::process::Command::new("sleep")
            .arg("100")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let instance = VMInstance::new("fixture", PathBuf::from("/unused"), None, Some(child));
        let watcher = tokio::spawn(instance.watch_child());
        instance
            .stopping
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(watcher.await.unwrap().unwrap().is_none());
        instance.stop_child_after_failed_start().await;
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }
}
