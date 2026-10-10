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
    fs::{self, File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use tokio::{sync::watch, task::JoinHandle};
use tracing::{debug, error, info, trace, warn};
use url::Url;

use crate::ch_driver::{
    provisioning::hooks::HookManager,
    transform::{
        ConfigTransform, StorageClaim, StorageCleanupContext, StorageOwnershipContext,
        StorageReleaseState, TransformChain,
    },
};

use super::api::call_request;

pub const CONFIG_FILE_NAME: &str = "config.json";
const SOCKET_FILE_NAME: &str = "ch.sock";
const CONSOLE_SOCKET_FILE_NAME: &str = "console.sock";
const SOCKET_LOCK_FILE_NAME: &str = "ch.sock.lock";
pub const VMS_DIR_NAME: &str = "vms";
pub type ConsoleStream = std::fs::File;

/// Coordinates shutdown with the external task watching an owned child process.
pub struct ChildProcessWatcher {
    stop_requested: watch::Receiver<bool>,
    exited: watch::Sender<bool>,
}

impl ChildProcessWatcher {
    /// Wait for the child, honoring teardown requests by killing and reaping it.
    pub async fn wait(
        mut self,
        mut child: tokio::process::Child,
    ) -> (std::io::Result<std::process::ExitStatus>, bool) {
        let (result, stopped_for_teardown) = tokio::select! {
            result = child.wait() => (result, false),
            changed = self.stop_requested.changed() => {
                if changed.is_ok() && *self.stop_requested.borrow() {
                    _ = child.start_kill();
                    (child.wait().await, true)
                } else {
                    (child.wait().await, false)
                }
            }
        };
        if result.is_ok() {
            _ = self.exited.send(true);
        }
        (result, stopped_for_teardown)
    }
}

const DEFAULT_RUNTIME_ROOT_DIR: &str = "/run/odorobo";
const RUNTIME_ROOT_ENV_VAR: &str = "ODOROBO_RUNTIME_DIR";
const RECOVERY_CONFIG_FILE_NAME: &str = "failed-start-config.json";
const STORAGE_CLEANUP_JOURNAL_FILE_NAME: &str = "storage-cleanup-journal.json";
const STORAGE_CLAIMS_DIR_NAME: &str = ".storage-claims";
const STORAGE_CLAIMS_LOCK_FILE_NAME: &str = ".lock";
static STORAGE_OWNER_GENERATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
const CH_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const CH_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CH_STARTUP_RETRY_INTERVAL: Duration = Duration::from_millis(250);

#[derive(serde::Serialize, serde::Deserialize)]
struct StorageOwnershipRecord {
    version: u32,
    owner_vm_id: String,
    resource_key: String,
    uri: String,
    #[serde(default)]
    owner_generation: String,
    journaled: bool,
    attachment: Option<String>,
    #[serde(default)]
    release_started: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SocketProbe {
    Responding,
    Dead,
    Uncertain,
}

pub struct VMInstance {
    pub id: String,
    pub ch_socket_path: PathBuf,
    transformer: TransformChain,
    hook_manager: HookManager,
    child_process: Option<tokio::process::Child>,
    /// True while this instance still owns the VMM process, including after
    /// the `Child` handle has been transferred to its watcher.
    owns_process: bool,
    process_stop: Option<watch::Sender<bool>>,
    process_exited: Option<watch::Receiver<bool>>,
    #[cfg(test)]
    runtime_dir_override: Option<PathBuf>,
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
            owns_process: child_process.is_some(),
            child_process,
            process_stop: None,
            process_exited: None,
            #[cfg(test)]
            runtime_dir_override: None,
            vm_config: None,
        }
    }

    /// Takes the child process out of this instance, transferring ownership to the caller.
    /// Prefer `take_child_process_for_watcher` to preserve a coordinated destroy path.
    pub const fn take_child_process(&mut self) -> Option<tokio::process::Child> {
        self.child_process.take()
    }

    /// Transfers the child to an external watcher while retaining a bounded
    /// shutdown path for `destroy()`.
    pub fn take_child_process_for_watcher(
        &mut self,
    ) -> Option<(tokio::process::Child, ChildProcessWatcher)> {
        let child = self.child_process.take()?;
        let (stop_tx, stop_rx) = watch::channel(false);
        let (exited_tx, exited_rx) = watch::channel(false);
        self.process_stop = Some(stop_tx);
        self.process_exited = Some(exited_rx);
        Some((
            child,
            ChildProcessWatcher {
                stop_requested: stop_rx,
                exited: exited_tx,
            },
        ))
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
        tokio::time::timeout(CH_REQUEST_TIMEOUT, self.conn().boot_vm())
            .await
            .map_err(|_| eyre!("Timed out booting VM {}", self.vm_id()))?
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
                    match tokio::time::timeout(CH_REQUEST_TIMEOUT, conn.shutdown_vmm()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            error!(vm_id, error = ?e, "Failed to shut down VM after migration");
                        }
                        Err(e) => {
                            error!(vm_id, error = ?e, "Timed out shutting down VM after migration");
                        }
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
        #[cfg(test)]
        if let Some(runtime_dir) = &self.runtime_dir_override {
            return runtime_dir.clone();
        }
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

    /// Returns whether a Cloud Hypervisor VMM is already serving this VM's
    /// runtime socket. The ping is bounded so a silent VMM cannot block recovery.
    pub async fn is_running(id: &str) -> bool {
        let socket = Self::runtime_dir_for(id).join(SOCKET_FILE_NAME);
        Self::probe_socket(&socket, CH_REQUEST_TIMEOUT).await == SocketProbe::Responding
    }

    async fn probe_socket(socket: &Path, timeout: Duration) -> SocketProbe {
        let result = tokio::time::timeout(
            timeout,
            cloud_hypervisor_client::socket_based_api_client(socket).vmm_ping_get(),
        )
        .await;
        match result {
            Ok(Ok(_)) => SocketProbe::Responding,
            Ok(Err(ChClientError::HyperClient(error))) if error.is_connect() => {
                match tokio::time::timeout(timeout, tokio::net::UnixStream::connect(socket)).await {
                    Ok(Err(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        ) =>
                    {
                        SocketProbe::Dead
                    }
                    Ok(Ok(_) | Err(_)) | Err(_) => SocketProbe::Uncertain,
                }
            }
            // API, decoding, non-connection transport errors, and deadline expiry
            // are ambiguous; never use them to unlink or replace a VMM.
            Ok(Err(_)) | Err(_) => SocketProbe::Uncertain,
        }
    }

    async fn prepare_runtime_dir_for_spawn(runtime_dir: &Path) -> Result<bool> {
        Self::prepare_runtime_dir_for_spawn_with_timeout(runtime_dir, CH_REQUEST_TIMEOUT).await
    }

    async fn prepare_runtime_dir_for_spawn_with_timeout(
        runtime_dir: &Path,
        timeout: Duration,
    ) -> Result<bool> {
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        match Self::probe_socket(&socket, timeout).await {
            SocketProbe::Responding => Ok(true),
            SocketProbe::Dead => {
                Self::purge_stale_runtime_sockets(runtime_dir)?;
                Ok(false)
            }
            SocketProbe::Uncertain => Err(eyre!(
                "Cannot determine whether Cloud Hypervisor is running for socket {}",
                socket.display()
            )),
        }
    }

    async fn reattach(
        id: &str,
        ch_socket_path: PathBuf,
        vm_config: Option<VmConfig>,
        boot: bool,
        transformer: Option<TransformChain>,
    ) -> Result<Self> {
        let mut instance = Self::new(id, ch_socket_path.clone(), transformer, None);
        #[cfg(test)]
        let () = {
            instance.runtime_dir_override = ch_socket_path.parent().map(Path::to_path_buf);
        };
        let info = tokio::time::timeout(CH_REQUEST_TIMEOUT, instance.conn().vm_info_get())
            .await
            .map_err(|_| eyre!("Timed out reading VM info while reattaching to {id}"))?;
        match info {
            Ok(info) => {
                let should_boot = boot && info.state == models::VmState::Created;
                instance.save_config(&info.config).wrap_err(eyre!(
                    "Failed to persist cleanup config while reattaching to {id}"
                ))?;
                instance.vm_config = Some(info.config);
                if should_boot {
                    instance
                        .boot()
                        .await
                        .wrap_err(eyre!("Failed to boot created VM while reattaching to {id}"))?;
                }
            }
            Err(ChClientError::Api(api)) if api.code == hyper::StatusCode::NOT_FOUND => {
                Self::cleanup_persisted_configs(
                    id,
                    &instance.runtime_dir(),
                    &instance.transformer,
                )?;
                if let Some(vm_config) = vm_config {
                    info!(vm_id = id, "Found an empty VMM; creating its VM config");
                    if let Err(error) = instance.create_config(vm_config, boot).await {
                        let shutdown = tokio::time::timeout(
                            CH_REQUEST_TIMEOUT,
                            instance.conn().shutdown_vmm(),
                        )
                        .await;
                        if matches!(shutdown, Ok(Ok(()))) {
                            if let Err(cleanup_error) = instance.purge_instance_data() {
                                instance.persist_failed_start_config()?;
                                return Err(eyre!(
                                    "Empty-VMM recovery failed ({error:#}); cleanup failed ({cleanup_error:#})"
                                ));
                            }
                        } else {
                            instance.persist_failed_start_config()?;
                        }
                        return Err(eyre!(
                            "Failed to initialize empty Cloud Hypervisor VMM: {error:#}"
                        ));
                    }
                }
            }
            Err(error) => {
                return Err(eyre!(ChApiError::from(error))
                    .wrap_err(eyre!("Failed to read VM config while reattaching to {id}")));
            }
        }
        Ok(instance)
    }

    fn persist_failed_start_config(&self) -> Result<()> {
        let Some(config) = self.vm_config.as_ref() else {
            return Ok(());
        };
        let path = self.runtime_dir().join(RECOVERY_CONFIG_FILE_NAME);
        Self::persist_config_file(&path, config)
            .wrap_err("Failed to persist failed-start recovery config")
    }

    fn persist_config_file(path: &Path, config: &VmConfig) -> Result<()> {
        Self::persist_json_file(path, config, "VM cleanup config")
    }

    fn persist_json_file<T: serde::Serialize>(
        path: &Path,
        value: &T,
        description: &str,
    ) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| eyre!("Metadata file path has no parent: {}", path.display()))?;
        fs::create_dir_all(parent).wrap_err("Failed to create VM runtime directory")?;
        let file_name = path
            .file_name()
            .ok_or_else(|| eyre!("Metadata file path has no file name: {}", path.display()))?
            .to_string_lossy();
        let temporary_path = parent.join(format!(".{file_name}.{}.tmp", ulid::Ulid::generate()));

        let result: Result<()> = (|| {
            let mut file = File::create(&temporary_path)
                .wrap_err_with(|| format!("Failed to create temporary {description}"))?;
            serde_json::to_writer_pretty(&mut file, value)
                .wrap_err_with(|| format!("Failed to serialize {description}"))?;
            file.sync_all()
                .wrap_err_with(|| format!("Failed to sync temporary {description}"))?;
            fs::rename(&temporary_path, path)
                .wrap_err_with(|| format!("Failed to publish {description}"))?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .wrap_err("Failed to sync VM runtime directory")?;
            Ok(())
        })();
        if result.is_err() {
            _ = fs::remove_file(temporary_path);
        }
        result
    }

    fn storage_cleanup_journal_path(runtime_dir: &Path) -> PathBuf {
        runtime_dir.join(STORAGE_CLEANUP_JOURNAL_FILE_NAME)
    }

    fn storage_claims_dir(runtime_dir: &Path) -> Result<PathBuf> {
        let vm_root = runtime_dir.parent().ok_or_else(|| {
            eyre!(
                "VM runtime directory has no parent: {}",
                runtime_dir.display()
            )
        })?;
        Ok(vm_root.join(STORAGE_CLAIMS_DIR_NAME))
    }

    fn with_storage_claim_lock<T>(
        runtime_dir: &Path,
        action: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let claims_dir = Self::storage_claims_dir(runtime_dir)?;
        fs::create_dir_all(&claims_dir).wrap_err("Failed to create storage ownership directory")?;
        let lock_path = claims_dir.join(STORAGE_CLAIMS_LOCK_FILE_NAME);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .wrap_err("Failed to open storage ownership lock")?;
        // The lock is node-local and protects atomic read/modify/write cycles
        // across VMs and independent agent processes on this host.
        // SAFETY: flock only operates on this valid open file descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .wrap_err("Failed to lock storage ownership registry");
        }
        action(&claims_dir)
    }

    fn storage_owner_generation() -> &'static str {
        STORAGE_OWNER_GENERATION.get_or_init(|| ulid::Ulid::generate().to_string())
    }

    fn storage_claim_file_name(resource_key: &str) -> String {
        // Stable FNV-1a makes the filename portable across agent restarts. The
        // complete key is checked inside the record, so a hash collision fails
        // closed rather than aliasing two resources.
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in resource_key.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{hash:016x}.json")
    }

    fn read_storage_claim(path: &Path) -> Result<Option<StorageOwnershipRecord>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).wrap_err("Failed to open storage ownership claim"),
        };
        serde_json::from_reader(file)
            .map(Some)
            .wrap_err("Failed to parse storage ownership claim")
    }

    fn write_storage_claim(path: &Path, record: &StorageOwnershipRecord) -> Result<()> {
        Self::persist_json_file(path, record, "storage ownership claim")
    }

    fn claim_storage_resource(
        runtime_dir: &Path,
        vm_id: &str,
        uri: &Url,
        resource_key: &str,
    ) -> Result<StorageClaim> {
        Self::with_storage_claim_lock(runtime_dir, |claims_dir| {
            let path = claims_dir.join(Self::storage_claim_file_name(resource_key));
            if let Some(record) = Self::read_storage_claim(&path)? {
                if record.version != 1 {
                    return Err(eyre!("Unsupported storage ownership claim version"));
                }
                if record.resource_key != resource_key {
                    return Err(eyre!(
                        "Storage ownership hash collision for resource {resource_key}"
                    ));
                }
                if record.owner_vm_id != vm_id {
                    return Err(eyre!(
                        "Storage resource {resource_key} is owned by VM {}; refusing cross-VM reuse",
                        record.owner_vm_id
                    ));
                }
                let journal_contains_claim = Self::load_storage_cleanup_journal(runtime_dir)?
                    .iter()
                    .any(|journal_uri| journal_uri.as_str() == record.uri);
                let recovered = record.owner_generation != Self::storage_owner_generation();
                Ok(StorageClaim {
                    is_new: false,
                    journaled: record.journaled || journal_contains_claim,
                    attachment: record.attachment,
                    recovered,
                    release_started: record.release_started,
                })
            } else {
                let record = StorageOwnershipRecord {
                    version: 1,
                    owner_vm_id: vm_id.to_owned(),
                    resource_key: resource_key.to_owned(),
                    uri: uri.as_str().to_owned(),
                    owner_generation: Self::storage_owner_generation().to_owned(),
                    journaled: false,
                    attachment: None,
                    release_started: false,
                };
                Self::write_storage_claim(&path, &record)?;
                Ok(StorageClaim {
                    is_new: true,
                    journaled: false,
                    attachment: None,
                    recovered: false,
                    release_started: false,
                })
            }
        })
    }

    fn update_storage_claim(
        runtime_dir: &Path,
        vm_id: &str,
        resource_key: &str,
        update: impl FnOnce(&mut StorageOwnershipRecord),
    ) -> Result<()> {
        Self::with_storage_claim_lock(runtime_dir, |claims_dir| {
            let path = claims_dir.join(Self::storage_claim_file_name(resource_key));
            let mut record = Self::read_storage_claim(&path)?
                .ok_or_else(|| eyre!("Storage ownership claim for {resource_key} is missing"))?;
            if record.resource_key != resource_key || record.owner_vm_id != vm_id {
                return Err(eyre!(
                    "VM {vm_id} does not own storage claim for {resource_key}"
                ));
            }
            update(&mut record);
            Self::write_storage_claim(&path, &record)
        })
    }

    fn abandon_storage_claim(runtime_dir: &Path, vm_id: &str, resource_key: &str) -> Result<()> {
        Self::with_storage_claim_lock(runtime_dir, |claims_dir| {
            let path = claims_dir.join(Self::storage_claim_file_name(resource_key));
            let Some(record) = Self::read_storage_claim(&path)? else {
                return Ok(());
            };
            if record.owner_vm_id != vm_id || record.resource_key != resource_key {
                return Err(eyre!("Refusing to abandon another VM's storage claim"));
            }
            let in_journal = Self::load_storage_cleanup_journal(runtime_dir)?
                .iter()
                .any(|uri| uri.as_str() == record.uri);
            if record.journaled || in_journal {
                return Err(eyre!(
                    "Cannot abandon journaled storage claim for {resource_key}"
                ));
            }
            fs::remove_file(&path).wrap_err("Failed to abandon storage ownership claim")?;
            File::open(claims_dir)
                .and_then(|directory| directory.sync_all())
                .wrap_err("Failed to sync storage ownership directory")
        })
    }

    fn remove_storage_claim(runtime_dir: &Path, vm_id: &str, resource_key: &str) -> Result<()> {
        Self::with_storage_claim_lock(runtime_dir, |claims_dir| {
            let path = claims_dir.join(Self::storage_claim_file_name(resource_key));
            let Some(record) = Self::read_storage_claim(&path)? else {
                return Ok(());
            };
            if record.owner_vm_id != vm_id || record.resource_key != resource_key {
                return Err(eyre!("Refusing to remove another VM's storage claim"));
            }
            fs::remove_file(&path).wrap_err("Failed to remove storage ownership claim")?;
            File::open(claims_dir)
                .and_then(|directory| directory.sync_all())
                .wrap_err("Failed to sync storage ownership directory")
        })
    }

    fn mark_storage_claim_journaled(
        runtime_dir: &Path,
        vm_id: &str,
        resource_key: &str,
    ) -> Result<()> {
        Self::update_storage_claim(runtime_dir, vm_id, resource_key, |record| {
            record.journaled = true;
        })
    }

    fn set_storage_claim_attachment(
        runtime_dir: &Path,
        vm_id: &str,
        resource_key: &str,
        attachment: &str,
    ) -> Result<()> {
        Self::update_storage_claim(runtime_dir, vm_id, resource_key, |record| {
            record.attachment = Some(attachment.to_owned());
        })
    }

    fn begin_storage_claim_release(
        runtime_dir: &Path,
        vm_id: &str,
        resource_key: &str,
    ) -> Result<StorageReleaseState> {
        Self::with_storage_claim_lock(runtime_dir, |claims_dir| {
            let path = claims_dir.join(Self::storage_claim_file_name(resource_key));
            let Some(mut record) = Self::read_storage_claim(&path)? else {
                // Legacy cleanup journals can predate node-local ownership
                // claims. Preserve their release path; they have no durable
                // phase or process generation to report.
                return Ok(StorageReleaseState::default());
            };
            if record.resource_key != resource_key || record.owner_vm_id != vm_id {
                return Err(eyre!(
                    "VM {vm_id} does not own storage claim for {resource_key}"
                ));
            }
            let state = StorageReleaseState {
                already_started: record.release_started,
                recovered: record.owner_generation != Self::storage_owner_generation(),
                claim_exists: true,
            };
            if !record.release_started {
                record.release_started = true;
                Self::write_storage_claim(&path, &record)?;
            }
            Ok(state)
        })
    }

    fn storage_claim_attachment(
        runtime_dir: &Path,
        vm_id: &str,
        resource_key: &str,
    ) -> Result<Option<String>> {
        Self::with_storage_claim_lock(runtime_dir, |claims_dir| {
            let path = claims_dir.join(Self::storage_claim_file_name(resource_key));
            let Some(record) = Self::read_storage_claim(&path)? else {
                return Ok(None);
            };
            if record.resource_key != resource_key || record.owner_vm_id != vm_id {
                return Err(eyre!(
                    "VM {vm_id} does not own storage claim for {resource_key}"
                ));
            }
            Ok(record.attachment)
        })
    }

    fn transform_with_storage_ownership(
        vm_id: &str,
        runtime_dir: &Path,
        transformer: &TransformChain,
        config: &mut VmConfig,
    ) -> Result<()> {
        let mut claim =
            |uri: &Url, key: &str| Self::claim_storage_resource(runtime_dir, vm_id, uri, key);
        let mut record = |uri: &Url| Self::record_storage_cleanup_uri(runtime_dir, uri);
        let mut mark_journaled =
            |_: &Url, key: &str| Self::mark_storage_claim_journaled(runtime_dir, vm_id, key);
        let mut set_attachment = |_: &Url, key: &str, attachment: &str| {
            Self::set_storage_claim_attachment(runtime_dir, vm_id, key, attachment)
        };
        let mut abandon = |_: &Url, key: &str| Self::abandon_storage_claim(runtime_dir, vm_id, key);
        let mut ownership = StorageOwnershipContext {
            claim: &mut claim,
            record: &mut record,
            mark_journaled: &mut mark_journaled,
            set_attachment: &mut set_attachment,
            abandon: &mut abandon,
        };
        transformer.transform_with_storage_ownership(vm_id, config, &mut ownership)
    }

    fn teardown_with_storage_journal(
        id: &str,
        runtime_dir: &Path,
        transformer: &TransformChain,
        config: &mut VmConfig,
    ) -> Result<()> {
        transformer.validate_storage_cleanup_metadata(
            id,
            config,
            Self::storage_cleanup_journal_path(runtime_dir).exists(),
        )?;
        let journal = Self::load_storage_cleanup_journal(runtime_dir)?;
        let mut attachment = |_: &Url, key: &str| {
            if key.is_empty() {
                Ok(None)
            } else {
                Self::storage_claim_attachment(runtime_dir, id, key)
            }
        };
        let mut begin_release = |_: &Url, key: &str| {
            if key.is_empty() {
                Ok(StorageReleaseState::default())
            } else {
                Self::begin_storage_claim_release(runtime_dir, id, key)
            }
        };
        let mut forget = |uri: &Url, key: &str| {
            Self::forget_storage_cleanup_uri(runtime_dir, uri)?;
            if key.is_empty() {
                Ok(())
            } else {
                Self::remove_storage_claim(runtime_dir, id, key)
            }
        };
        let mut cleanup = StorageCleanupContext {
            attachment: &mut attachment,
            begin_release: &mut begin_release,
            forget: &mut forget,
        };
        transformer.teardown_with_storage_ownership(id, config, &journal, &mut cleanup)
    }

    fn cleanup_orphan_storage_claims(vm_id: &str, runtime_dir: &Path) -> Result<()> {
        let journaled_uris = Self::load_storage_cleanup_journal(runtime_dir)?
            .into_iter()
            .map(|uri| uri.to_string())
            .collect::<std::collections::HashSet<_>>();
        Self::with_storage_claim_lock(runtime_dir, |claims_dir| {
            for entry in
                fs::read_dir(claims_dir).wrap_err("Failed to scan storage ownership claims")?
            {
                let entry = entry.wrap_err("Failed to read storage ownership claim entry")?;
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                    continue;
                }
                let Some(record) = Self::read_storage_claim(&path)? else {
                    continue;
                };
                if record.owner_vm_id == vm_id && !journaled_uris.contains(&record.uri) {
                    fs::remove_file(&path)
                        .wrap_err("Failed to remove orphaned storage ownership claim")?;
                }
            }
            File::open(claims_dir)
                .and_then(|directory| directory.sync_all())
                .wrap_err("Failed to sync storage ownership directory")
        })
    }

    fn load_storage_cleanup_journal(runtime_dir: &Path) -> Result<Vec<Url>> {
        let path = Self::storage_cleanup_journal_path(runtime_dir);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).wrap_err("Failed to open storage cleanup journal"),
        };
        let entries: Vec<String> =
            serde_json::from_reader(file).wrap_err("Failed to parse storage cleanup journal")?;
        entries
            .into_iter()
            .map(|entry| {
                Url::parse(&entry)
                    .wrap_err_with(|| format!("Invalid URI in storage cleanup journal: {entry}"))
            })
            .collect()
    }

    fn persist_storage_cleanup_journal(runtime_dir: &Path, entries: &[Url]) -> Result<()> {
        let entries = entries
            .iter()
            .map(|uri| uri.as_str().to_owned())
            .collect::<Vec<_>>();
        Self::persist_json_file(
            &Self::storage_cleanup_journal_path(runtime_dir),
            &entries,
            "storage cleanup journal",
        )
    }

    fn ensure_storage_cleanup_journal(runtime_dir: &Path) -> Result<()> {
        let path = Self::storage_cleanup_journal_path(runtime_dir);
        if path.exists() {
            // Fail closed on corrupt metadata before making any new attachment.
            Self::load_storage_cleanup_journal(runtime_dir)?;
        } else {
            Self::persist_storage_cleanup_journal(runtime_dir, &[])?;
        }
        Ok(())
    }

    fn record_storage_cleanup_uri(runtime_dir: &Path, uri: &Url) -> Result<()> {
        let mut entries = Self::load_storage_cleanup_journal(runtime_dir)?;
        if !entries.iter().any(|entry| entry == uri) {
            entries.push(uri.clone());
            Self::persist_storage_cleanup_journal(runtime_dir, &entries)?;
        }
        Ok(())
    }

    fn forget_storage_cleanup_uri(runtime_dir: &Path, uri: &Url) -> Result<()> {
        let mut entries = Self::load_storage_cleanup_journal(runtime_dir)?;
        entries.retain(|entry| entry != uri);
        Self::persist_storage_cleanup_journal(runtime_dir, &entries)
    }

    fn cleanup_persisted_configs(
        id: &str,
        runtime_dir: &Path,
        transformer: &TransformChain,
    ) -> Result<()> {
        let mut failures = Vec::new();
        let mut teardown_attempted = false;
        for name in [RECOVERY_CONFIG_FILE_NAME, CONFIG_FILE_NAME] {
            let path = runtime_dir.join(name);
            let mut file = match File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    failures.push(format!(
                        "{}: failed to open config: {error}",
                        path.display()
                    ));
                    continue;
                }
            };
            let mut config: VmConfig = match serde_json::from_reader(&mut file) {
                Ok(config) => config,
                Err(error) => {
                    failures.push(format!(
                        "{}: failed to parse config: {error}",
                        path.display()
                    ));
                    continue;
                }
            };
            teardown_attempted = true;
            if let Err(error) =
                Self::teardown_with_storage_journal(id, runtime_dir, transformer, &mut config)
            {
                failures.push(format!("{}: {error:#}", path.display()));
                continue;
            }
            if let Err(error) = fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                failures.push(format!(
                    "{}: failed to remove config: {error}",
                    path.display()
                ));
            }
        }
        let journal_path = Self::storage_cleanup_journal_path(runtime_dir);
        if !teardown_attempted && journal_path.exists() {
            let mut config = VmConfig::default();
            if let Err(error) =
                Self::teardown_with_storage_journal(id, runtime_dir, transformer, &mut config)
            {
                failures.push(format!("{}: {error:#}", journal_path.display()));
            }
        }
        if failures.is_empty() {
            Self::cleanup_orphan_storage_claims(id, runtime_dir)?;
            let journal_path = Self::storage_cleanup_journal_path(runtime_dir);
            if let Err(error) = fs::remove_file(&journal_path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error).wrap_err("Failed to remove empty storage cleanup journal");
            }
            Ok(())
        } else {
            Err(eyre!(
                "Failed to clean up persisted VM config(s): {}",
                failures.join("; ")
            ))
        }
    }

    fn purge_stale_runtime_sockets(runtime_dir: &Path) -> Result<()> {
        for name in [
            SOCKET_FILE_NAME,
            CONSOLE_SOCKET_FILE_NAME,
            SOCKET_LOCK_FILE_NAME,
        ] {
            let path = runtime_dir.join(name);
            match fs::remove_file(&path) {
                Ok(()) => debug!(?path, "Removed stale VM runtime socket"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).wrap_err_with(|| {
                        eyre!(
                            "Failed to remove stale VM runtime artifact {}",
                            path.display()
                        )
                    });
                }
            }
        }
        Ok(())
    }

    async fn stop_child_after_failed_start(&mut self) {
        if let Some(mut child) = self.child_process.take() {
            _ = child.start_kill();
            _ = child.wait().await;
        }
    }

    async fn stop_owned_process(&mut self) -> Result<()> {
        if let Some(mut child) = self.child_process.take() {
            _ = child.start_kill();
            child
                .wait()
                .await
                .wrap_err("Failed to wait for VMM child process")?;
            return Ok(());
        }
        if !self.owns_process {
            return Ok(());
        }
        let stop = self
            .process_stop
            .as_ref()
            .ok_or_else(|| eyre!("Owned VMM process was transferred without a shutdown watcher"))?;
        let exited = self
            .process_exited
            .as_mut()
            .ok_or_else(|| eyre!("Owned VMM process watcher is missing its exit signal"))?;
        if !*exited.borrow() {
            _ = stop.send(true);
            tokio::time::timeout(CH_REQUEST_TIMEOUT, async {
                loop {
                    if *exited.borrow() || exited.changed().await.is_err() {
                        return;
                    }
                }
            })
            .await
            .map_err(|_| eyre!("Timed out waiting for owned VMM process to exit"))?;
        }
        if !*exited.borrow() {
            return Err(eyre!("VMM watcher exited without confirming process death"));
        }
        Ok(())
    }

    pub async fn info(&self) -> Result<VmInfo> {
        tokio::time::timeout(CH_REQUEST_TIMEOUT, self.conn().vm_info_get())
            .await
            .map_err(|_| eyre!("Timed out getting VM info for {}", self.vm_id()))?
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to get VM info for {}", self.vm_id()))
    }

    pub async fn shutdown(&self) -> Result<()> {
        tokio::time::timeout(CH_REQUEST_TIMEOUT, self.conn().shutdown_vm())
            .await
            .map_err(|_| eyre!("Timed out shutting down VM {}", self.vm_id()))?
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
        tokio::time::timeout(CH_REQUEST_TIMEOUT, self.conn().vmm_ping_get())
            .await
            .map_err(|_| eyre!("Timed out pinging VM {}", self.vm_id()))?
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
        let runtime_dir = ch_socket_path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&runtime_dir)?;
        if Self::prepare_runtime_dir_for_spawn(&runtime_dir).await? {
            info!(vm_id = id, "Attaching to existing Cloud Hypervisor VMM");
            return Self::reattach(id, ch_socket_path, vm_config, boot, transformer).await;
        }
        let transformer = transformer.unwrap_or_default();
        Self::cleanup_persisted_configs(id, &runtime_dir, &transformer)?;

        let ch_process = tokio::process::Command::new("cloud-hypervisor")
            .arg("--api-socket")
            .arg(&ch_socket_path)
            .spawn()?;
        let mut instance = Self::new(id, ch_socket_path, Some(transformer), Some(ch_process));

        let startup_deadline = tokio::time::Instant::now()
            .checked_add(CH_STARTUP_TIMEOUT)
            .ok_or_else(|| eyre!("CH startup deadline overflow"))?;
        while tokio::time::Instant::now() < startup_deadline {
            let remaining = startup_deadline.saturating_duration_since(tokio::time::Instant::now());
            if Self::probe_socket(&instance.ch_socket_path, remaining.min(CH_REQUEST_TIMEOUT)).await
                == SocketProbe::Responding
            {
                if let Some(vm_config) = vm_config
                    && let Err(error) = instance.create_config(vm_config, boot).await
                {
                    instance.stop_child_after_failed_start().await;
                    if let Err(cleanup_error) = instance.purge_instance_data() {
                        instance.persist_failed_start_config()?;
                        return Err(eyre!(
                            "Failed startup cleanup ({cleanup_error:#}); original error: {error:#}"
                        ));
                    }
                    return Err(error);
                }
                return Ok(instance);
            }
            let remaining = startup_deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(remaining.min(CH_STARTUP_RETRY_INTERVAL)).await;
        }

        instance.stop_child_after_failed_start().await;
        Self::purge_stale_runtime_sockets(&runtime_dir)?;
        Err(eyre!(
            "CH socket did not become available within {:?} for VM {}",
            CH_STARTUP_TIMEOUT,
            id
        ))
    }

    async fn inspect_vmm_before_destroy(&self) -> Result<bool> {
        match tokio::time::timeout(CH_REQUEST_TIMEOUT, self.conn().vm_info_get()).await {
            Ok(Ok(info)) => {
                self.hook_manager.before_stop(self.vm_id(), &info).await?;
                if matches!(
                    info.state,
                    models::VmState::Running | models::VmState::Paused
                ) && let Err(error) = self.shutdown().await
                {
                    if self.owns_process {
                        warn!(
                            vm_id = self.vm_id(),
                            ?error,
                            "VM shutdown failed; terminating owned VMM"
                        );
                        return Ok(true);
                    }
                    return Err(eyre!("Failed to shut down VM before deletion: {error:#}"));
                }
                Ok(false)
            }
            Ok(Err(ChClientError::Api(api))) if api.code == hyper::StatusCode::NOT_FOUND => {
                // An empty but responsive VMM is valid (e.g. a migration receiver).
                Ok(false)
            }
            result => {
                let failure = match result {
                    Ok(Err(error)) => ChApiError::from(error).to_string(),
                    Err(_) => format!("timed out getting VM info for {}", self.vm_id()),
                    Ok(Ok(_)) => unreachable!(),
                };
                if self.owns_process {
                    if self.child_process.is_some() || self.process_stop.is_some() {
                        warn!(vm_id = self.vm_id(), %failure, "Cannot query owned VMM; terminating it");
                        return Ok(true);
                    }
                    return Err(eyre!("Owned VMM has no watcher control; refusing cleanup"));
                }
                match Self::probe_socket(&self.ch_socket_path, CH_REQUEST_TIMEOUT).await {
                    SocketProbe::Dead => Ok(true),
                    SocketProbe::Responding | SocketProbe::Uncertain => {
                        Err(eyre!(failure).wrap_err("Cannot confirm VMM state before deletion"))
                    }
                }
            }
        }
    }

    async fn request_vmm_shutdown_for_destroy(&self) -> Result<()> {
        match tokio::time::timeout(CH_REQUEST_TIMEOUT, self.conn().shutdown_vmm()).await {
            Ok(Ok(())) => {
                debug!(vm_id = self.vm_id(), "VMM shutdown successfully");
                Ok(())
            }
            Ok(Err(error))
                if self.owns_process
                    && (self.child_process.is_some() || self.process_stop.is_some()) =>
            {
                warn!(
                    vm_id = self.vm_id(),
                    ?error,
                    "VMM shutdown failed; terminating owned process"
                );
                Ok(())
            }
            Err(_)
                if self.owns_process
                    && (self.child_process.is_some() || self.process_stop.is_some()) =>
            {
                warn!(
                    vm_id = self.vm_id(),
                    "VMM shutdown timed out; terminating owned process"
                );
                Ok(())
            }
            Ok(Err(error)) if self.owns_process => {
                Err(eyre!(ChApiError::from(error)).wrap_err("Owned VMM has no watcher control"))
            }
            Err(_) if self.owns_process => Err(eyre!("Owned VMM has no watcher control")),
            Ok(Err(error)) => {
                match Self::probe_socket(&self.ch_socket_path, CH_REQUEST_TIMEOUT).await {
                    SocketProbe::Dead => Ok(()),
                    SocketProbe::Responding | SocketProbe::Uncertain => {
                        Err(eyre!(ChApiError::from(error))
                            .wrap_err("Failed to confirm attached VMM shutdown"))
                    }
                }
            }
            Err(_) => match Self::probe_socket(&self.ch_socket_path, CH_REQUEST_TIMEOUT).await {
                SocketProbe::Dead => Ok(()),
                SocketProbe::Responding | SocketProbe::Uncertain => {
                    Err(eyre!("Timed out confirming attached VMM shutdown"))
                }
            },
        }
    }

    /// Gracefully shut down the VM and VMM, then clean up runtime state.
    pub async fn destroy(&mut self) -> Result<()> {
        info!(vm_id = self.vm_id(), "Destroying VM instance");
        if let Some(config) = self.vm_config.as_ref() {
            self.save_config(config)
                .wrap_err("Failed to persist VM cleanup config before shutdown")?;
        }
        if !self.inspect_vmm_before_destroy().await? {
            self.request_vmm_shutdown_for_destroy().await?;
        }
        let vm_config = self.vm_config.clone().unwrap_or_default();
        self.stop_owned_process().await?;
        self.hook_manager
            .after_stop(self.vm_id(), &vm_config)
            .await?;
        self.purge_instance_data()
            .wrap_err("Failed to purge VM runtime data")?;
        Ok(())
    }

    /// Purge the runtime data for this VM instance.
    ///
    /// This removes the runtime directory and all its contents if it exists.
    pub fn purge_instance_data(&mut self) -> Result<()> {
        let mut vm_config = self.vm_config.clone().unwrap_or_default();
        if self.vm_config.is_some() {
            self.save_config(&vm_config)
                .wrap_err("Failed to persist VM cleanup config before teardown")?;
        }
        Self::teardown_with_storage_journal(
            self.vm_id(),
            &self.runtime_dir(),
            &self.transformer,
            &mut vm_config,
        )?;
        let runtime_dir = self.runtime_dir();
        Self::cleanup_orphan_storage_claims(self.vm_id(), &runtime_dir)?;
        if runtime_dir.exists() {
            fs::remove_dir_all(runtime_dir).wrap_err(eyre!(
                "Failed to remove runtime directory for {}",
                self.vm_id()
            ))?;
        }
        self.vm_config = None;
        Ok(())
    }

    /// Load the persisted VM config and its storage cleanup identifiers.
    pub fn load_config(&self) -> Result<models::VmConfig> {
        serde_json::from_reader(
            File::open(self.config_path())
                .wrap_err(eyre!("Failed to read config file for {}", self.vm_id()))?,
        )
        .wrap_err(eyre!("Failed to parse config JSON for {}", self.vm_id()))
    }

    /// Atomically persist VM config metadata for restart-safe resource cleanup.
    pub fn save_config(&self, config: &models::VmConfig) -> Result<()> {
        Self::persist_config_file(&self.config_path(), config)
            .wrap_err(eyre!("Failed to persist config file for {}", self.vm_id()))
    }

    /// Create and boot a VM with the given config.
    ///
    /// Applies node-specific transforms, saves config to disk, then:
    /// 1. Creates the VM via CH API
    /// 2. Boots the VM (if boot is true)
    pub async fn create_config(&mut self, config: models::VmConfig, boot: bool) -> Result<()> {
        trace!(vm_id = self.vm_id(), "Creating VM with provided config");
        self.save_config(&config).wrap_err(eyre!(
            "Failed to persist input cleanup config for VM {}",
            self.vm_id()
        ))?;

        trace!(vm_id = self.vm_id(), "Applying config transforms");
        let runtime_dir = self.runtime_dir();
        Self::ensure_storage_cleanup_journal(&runtime_dir)?;
        let mut transformed_config = config;
        if let Err(error) = Self::transform_with_storage_ownership(
            self.vm_id(),
            &runtime_dir,
            &self.transformer,
            &mut transformed_config,
        ) {
            self.vm_config = Some(transformed_config);
            return Err(error).wrap_err(eyre!(
                "Failed to apply config transforms for VM {}",
                self.vm_id()
            ));
        }
        self.vm_config = Some(transformed_config.clone());
        self.save_config(&transformed_config).wrap_err(eyre!(
            "Failed to persist cleanup config for VM {}",
            self.vm_id()
        ))?;

        trace!(vm_id = self.vm_id(), "Creating VM via CH API");
        tokio::time::timeout(
            CH_REQUEST_TIMEOUT,
            self.conn().create_vm(transformed_config),
        )
        .await
        .map_err(|_| eyre!("Timed out creating VM {}", self.vm_id()))?
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

        let runtime_dir = self.runtime_dir();
        Self::ensure_storage_cleanup_journal(&runtime_dir)?;
        let mut transformed_config = config.clone();
        Self::transform_with_storage_ownership(
            self.vm_id(),
            &runtime_dir,
            &self.transformer,
            &mut transformed_config,
        )
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
        tokio::time::timeout(CH_REQUEST_TIMEOUT, conn.delete_vm())
            .await
            .map_err(|_| eyre!("Timed out deleting VM {}", self.vm_id()))?
            .map_err(ChApiError::from)
            .wrap_err(eyre!("Failed to delete VM {}", self.vm_id()))?;
        // Retain local cleanup metadata until purge_instance_data has released resources.
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
    use super::{
        CONFIG_FILE_NAME, CONSOLE_SOCKET_FILE_NAME, SOCKET_FILE_NAME, SOCKET_LOCK_FILE_NAME,
        TransformChain, VMInstance,
    };
    use crate::ch_driver::transform::{
        ConfigTransform,
        storage::{StorageDriver, StorageDriverTransformer},
    };
    use async_trait::async_trait;
    use cloud_hypervisor_client::models::{DiskConfig, VmConfig, VmInfo, VmState};
    use stable_eyre::{Result, eyre::eyre};
    use std::{
        collections::BTreeSet,
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{UnixListener, UnixStream},
        process::Command,
    };

    fn temp_runtime_dir(label: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("odorobo-{label}-{}", ulid::Ulid::generate()))
            .join("vms")
            .join("vm")
    }

    async fn request_target(stream: &mut UnixStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).await.expect("read HTTP request");
            assert_ne!(read, 0, "client closed before sending HTTP headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break end.saturating_add(4);
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while request.len() < header_end.saturating_add(content_length) {
            let read = stream.read(&mut buffer).await.expect("read HTTP body");
            assert_ne!(read, 0, "client closed before sending HTTP body");
            request.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8_lossy(&request)
            .lines()
            .next()
            .expect("HTTP request line")
            .split_whitespace()
            .nth(1)
            .expect("HTTP target")
            .to_owned()
    }

    async fn respond(stream: &mut UnixStream, status: u16, body: &[u8]) {
        let reason = match status {
            200 => "OK",
            404 => "Not Found",
            _ => "Error",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write HTTP response headers");
        stream
            .write_all(body)
            .await
            .expect("write HTTP response body");
        stream.flush().await.expect("flush HTTP response");
    }

    #[derive(Clone)]
    struct RecordingStorage(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl StorageDriver for RecordingStorage {
        fn scheme(&self) -> &'static str {
            "teststorage"
        }

        async fn resolve(&self, _uri: &url::Url) -> Result<PathBuf> {
            Ok(PathBuf::from("/dev/test-storage"))
        }

        async fn release(&self, uri: &url::Url) -> Result<()> {
            let mut releases = self.0.lock().unwrap();
            releases.push(uri.to_string());
            drop(releases);
            Ok(())
        }
    }

    #[derive(Default)]
    struct AcquisitionState {
        resolve_attempts: Vec<String>,
        release_attempts: Vec<String>,
        active: BTreeSet<String>,
        fail_uri: Option<String>,
    }

    #[derive(Clone)]
    struct AcquisitionStorage(Arc<Mutex<AcquisitionState>>);

    #[async_trait]
    impl StorageDriver for AcquisitionStorage {
        fn scheme(&self) -> &'static str {
            "teststorage"
        }

        async fn resolve(&self, uri: &url::Url) -> Result<PathBuf> {
            let mut state = self.0.lock().unwrap();
            let uri = uri.to_string();
            state.resolve_attempts.push(uri.clone());
            // Model a backend whose command acquired the resource before it
            // returned an error: cleanup must retain this ambiguous attempt.
            state.active.insert(uri.clone());
            if state.fail_uri.as_deref() == Some(uri.as_str()) {
                drop(state);
                return Err(eyre!("simulated attach failure after side effect"));
            }
            drop(state);
            Ok(PathBuf::from("/dev/test-storage"))
        }

        async fn release(&self, uri: &url::Url) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            let uri = uri.to_string();
            state.release_attempts.push(uri.clone());
            state.active.remove(&uri);
            drop(state);
            Ok(())
        }
    }

    fn storage_config(uris: &[&str]) -> VmConfig {
        VmConfig {
            disks: Some(
                uris.iter()
                    .map(|uri| DiskConfig {
                        path: Some((*uri).to_owned()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    fn tracked_storage_transformer(state: Arc<Mutex<AcquisitionState>>) -> TransformChain {
        TransformChain::new()
            .add(StorageDriverTransformer::new().with_backend(AcquisitionStorage(state)))
    }

    fn transformed_disk_config() -> VmConfig {
        VmConfig {
            disks: Some(vec![DiskConfig {
                path: Some("/dev/rbd/pool/image".to_owned()),
                id: Some("rbd://pool/image?id=original-disk-id".to_owned()),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn stale_socket_cleanup_preserves_vm_config() {
        let runtime_dir = temp_runtime_dir("stale-sockets");
        fs::create_dir_all(&runtime_dir).expect("create temp runtime dir");
        for name in [
            SOCKET_FILE_NAME,
            CONSOLE_SOCKET_FILE_NAME,
            SOCKET_LOCK_FILE_NAME,
        ] {
            fs::write(runtime_dir.join(name), b"stale").expect("create stale socket placeholder");
        }
        let config_path = runtime_dir.join(CONFIG_FILE_NAME);
        fs::write(&config_path, b"{}").expect("create VM config");

        VMInstance::purge_stale_runtime_sockets(&runtime_dir).expect("purge stale sockets");

        for name in [
            SOCKET_FILE_NAME,
            CONSOLE_SOCKET_FILE_NAME,
            SOCKET_LOCK_FILE_NAME,
        ] {
            assert!(!runtime_dir.join(name).exists(), "{name} should be removed");
        }
        assert!(config_path.exists(), "VM config should be preserved");
        fs::remove_dir_all(runtime_dir).expect("remove temp runtime dir");
    }

    #[test]
    fn recovered_storage_claims_preserve_process_generation_uncertainty() {
        let runtime_dir = temp_runtime_dir("recovered-claim-generation");
        fs::create_dir_all(&runtime_dir).expect("create runtime directory");
        let vm_id = "recovered-generation";
        let uri = url::Url::parse("rbd://pool/image").unwrap();
        let resource_key = "rbd:pool/image";
        let record = super::StorageOwnershipRecord {
            version: 1,
            owner_vm_id: vm_id.to_owned(),
            resource_key: resource_key.to_owned(),
            uri: uri.as_str().to_owned(),
            owner_generation: "previous-agent-process".to_owned(),
            journaled: true,
            attachment: Some("/dev/rbd0".to_owned()),
            release_started: false,
        };
        let claims_dir = VMInstance::storage_claims_dir(&runtime_dir).unwrap();
        fs::create_dir_all(&claims_dir).expect("create claims directory");
        let claim_path = claims_dir.join(VMInstance::storage_claim_file_name(resource_key));
        fs::write(&claim_path, serde_json::to_vec(&record).unwrap())
            .expect("persist previous-process claim");
        VMInstance::persist_storage_cleanup_journal(&runtime_dir, std::slice::from_ref(&uri))
            .expect("persist cleanup journal");

        let claim = VMInstance::claim_storage_resource(&runtime_dir, vm_id, &uri, resource_key)
            .expect("read recovered claim");
        assert!(claim.recovered);
        assert!(!claim.release_started);
        let release =
            VMInstance::begin_storage_claim_release(&runtime_dir, vm_id, resource_key).unwrap();
        assert!(release.recovered);
        assert!(!release.already_started);
        fs::remove_dir_all(runtime_dir.parent().unwrap().parent().unwrap())
            .expect("remove recovered claim fixture");
    }

    #[test]
    fn legacy_storage_config_without_a_durable_journal_fails_closed() {
        for durable_empty_journal in [false, true] {
            let label = if durable_empty_journal {
                "legacy-storage-empty-journal"
            } else {
                "legacy-storage-missing-journal"
            };
            let runtime_dir = temp_runtime_dir(label);
            fs::create_dir_all(&runtime_dir).expect("create runtime directory");
            let vm_id = format!("{label}-vm");
            let mut instance = VMInstance::new(
                &vm_id,
                runtime_dir.join(SOCKET_FILE_NAME),
                Some(TransformChain::new()),
                None,
            );
            instance.runtime_dir_override = Some(runtime_dir.clone());
            let config = VmConfig {
                disks: Some(vec![DiskConfig {
                    path: Some("/dev/test-storage".to_owned()),
                    id: Some("teststorage://pool/disk?id=original".to_owned()),
                    ..Default::default()
                }]),
                ..Default::default()
            };
            instance
                .save_config(&config)
                .expect("persist legacy config");
            if durable_empty_journal {
                VMInstance::persist_storage_cleanup_journal(&runtime_dir, &[])
                    .expect("persist known-empty cleanup journal");
            }
            let releases = Arc::new(Mutex::new(Vec::new()));
            let transformer = TransformChain::new().add(
                StorageDriverTransformer::new()
                    .with_backend(RecordingStorage(Arc::clone(&releases))),
            );
            let result = VMInstance::cleanup_persisted_configs(&vm_id, &runtime_dir, &transformer);
            if durable_empty_journal {
                result.expect("durable empty journal proves no storage acquisition");
                assert!(!runtime_dir.join(CONFIG_FILE_NAME).exists());
            } else {
                let error = result.expect_err("missing journal must retain legacy metadata");
                assert!(
                    error.to_string().contains("journal is missing"),
                    "{error:#}"
                );
                assert!(runtime_dir.join(CONFIG_FILE_NAME).exists());
                assert!(
                    !runtime_dir.join("storage-cleanup-journal.json").exists(),
                    "cleanup must not manufacture an empty journal for legacy state"
                );
            }
            assert!(releases.lock().unwrap().is_empty());
            fs::remove_dir_all(runtime_dir.parent().unwrap().parent().unwrap())
                .expect("remove legacy config fixture");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dead_vmm_recovery_releases_persisted_storage_metadata_after_reconstruction() {
        let runtime_dir = temp_runtime_dir("persisted-cleanup");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let mut initial_instance = VMInstance::new(
            "persisted-cleanup",
            socket.clone(),
            Some(TransformChain::new()),
            None,
        );
        initial_instance.runtime_dir_override = Some(runtime_dir.clone());
        let uris = [
            "teststorage://pool/disk-one?id=original-one",
            "teststorage://pool/disk-two?id=original-two",
        ];
        let config = VmConfig {
            disks: Some(
                uris.iter()
                    .map(|uri| DiskConfig {
                        path: Some("/dev/test-storage".to_owned()),
                        id: Some((*uri).to_owned()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        };
        initial_instance
            .save_config(&config)
            .expect("persist transformed cleanup config");
        VMInstance::persist_storage_cleanup_journal(
            &runtime_dir,
            &uris
                .iter()
                .map(|uri| url::Url::parse(uri).unwrap())
                .collect::<Vec<_>>(),
        )
        .expect("persist acquired storage journal");
        drop(initial_instance);

        let listener = UnixListener::bind(&socket).expect("bind stale VMM socket");
        drop(listener);
        assert!(
            !VMInstance::prepare_runtime_dir_for_spawn(&runtime_dir)
                .await
                .expect("detect dead VMM")
        );

        let released = Arc::new(Mutex::new(Vec::new()));
        let transformer = TransformChain::new().add(
            StorageDriverTransformer::new().with_backend(RecordingStorage(Arc::clone(&released))),
        );
        VMInstance::cleanup_persisted_configs("persisted-cleanup", &runtime_dir, &transformer)
            .expect("recover and release persisted cleanup metadata");

        let released_uris = released.lock().unwrap().clone();
        assert_eq!(released_uris, uris.map(str::to_owned));
        assert!(
            !runtime_dir.join(CONFIG_FILE_NAME).exists(),
            "cleanup config is removed only after release succeeds"
        );
        fs::remove_dir_all(runtime_dir).expect("remove runtime dir");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_first_disk_never_releases_untouched_later_target_on_cleanup_or_restart() {
        let first_uri = "teststorage://first/disk";
        let second_uri = "teststorage://other-vm/disk";
        for restart_cleanup in [false, true] {
            let label = if restart_cleanup {
                "journal-restart"
            } else {
                "journal-immediate"
            };
            let runtime_dir = temp_runtime_dir(label);
            let vm_id = format!("{label}-{}", ulid::Ulid::generate());
            let state = Arc::new(Mutex::new(AcquisitionState {
                active: std::iter::once(second_uri.to_owned()).collect(),
                fail_uri: Some(first_uri.to_owned()),
                ..Default::default()
            }));
            let mut instance = VMInstance::new(
                &vm_id,
                runtime_dir.join(SOCKET_FILE_NAME),
                Some(tracked_storage_transformer(Arc::clone(&state))),
                None,
            );
            instance.runtime_dir_override = Some(runtime_dir.clone());
            fs::create_dir_all(&runtime_dir).expect("create runtime dir");
            let error = instance
                .create_config(storage_config(&[first_uri, second_uri]), false)
                .await
                .expect_err("first attach is configured to fail");
            assert!(format!("{error:#}").contains("simulated attach failure"));

            if restart_cleanup {
                drop(instance);
                VMInstance::cleanup_persisted_configs(
                    &vm_id,
                    &runtime_dir,
                    &tracked_storage_transformer(Arc::clone(&state)),
                )
                .expect("restart cleanup releases only journaled attempts");
            } else {
                instance
                    .purge_instance_data()
                    .expect("immediate cleanup releases only journaled attempts");
            }

            let state = state.lock().unwrap();
            assert_eq!(state.resolve_attempts, [first_uri]);
            assert_eq!(state.release_attempts, [first_uri]);
            assert_eq!(
                state.active,
                std::iter::once(second_uri.to_owned()).collect()
            );
            drop(state);
            if runtime_dir.exists() {
                fs::remove_dir_all(runtime_dir).expect("remove runtime dir");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn durable_node_claim_blocks_competing_vm_until_release_and_journal_forget_complete() {
        let runtime_a = temp_runtime_dir("shared-claim");
        let runtime_b = runtime_a.with_file_name("vm-b");
        let vm_a = format!("claim-a-{}", ulid::Ulid::generate());
        let vm_b = format!("claim-b-{}", ulid::Ulid::generate());
        let uri = "teststorage://shared/image";
        let resource = url::Url::parse(uri).unwrap();
        let state = Arc::new(Mutex::new(AcquisitionState::default()));
        let mut instance_a = VMInstance::new(
            &vm_a,
            runtime_a.join(SOCKET_FILE_NAME),
            Some(tracked_storage_transformer(Arc::clone(&state))),
            None,
        );
        instance_a.runtime_dir_override = Some(runtime_a.clone());
        let mut instance_b = VMInstance::new(
            &vm_b,
            runtime_b.join(SOCKET_FILE_NAME),
            Some(tracked_storage_transformer(Arc::clone(&state))),
            None,
        );
        instance_b.runtime_dir_override = Some(runtime_b.clone());

        let error = instance_a
            .create_config(storage_config(&[uri]), false)
            .await
            .expect_err("mock VMM socket is absent after storage acquisition");
        assert!(format!("{error:#}").contains("Failed to create VM"));
        let error = instance_b
            .create_config(storage_config(&[uri]), false)
            .await
            .expect_err("another VM must be denied by the durable ownership claim");
        assert!(format!("{error:#}").contains("owned by VM"));
        instance_b
            .purge_instance_data()
            .expect("rejected VM cleanup must not release the first VM's resource");
        assert_eq!(state.lock().unwrap().resolve_attempts, [uri]);

        // Model A's release command succeeding immediately before a crash that
        // prevents the journal forget. The stale claim must still fence B.
        AcquisitionStorage(Arc::clone(&state))
            .release(&resource)
            .await
            .expect("simulate successful release before interrupted journal update");
        drop(instance_a);
        let error = instance_b
            .create_config(storage_config(&[uri]), false)
            .await
            .expect_err("unresolved release claim must continue fencing other VMs");
        assert!(format!("{error:#}").contains("owned by VM"));

        VMInstance::cleanup_persisted_configs(
            &vm_a,
            &runtime_a,
            &tracked_storage_transformer(Arc::clone(&state)),
        )
        .expect("restart recovery completes journal forget and removes the claim");
        let error = instance_b
            .create_config(storage_config(&[uri]), false)
            .await
            .expect_err("VMM create should fail only after B acquires the now-free resource");
        assert!(format!("{error:#}").contains("Failed to create VM"));
        assert_eq!(state.lock().unwrap().resolve_attempts, [uri, uri]);
        instance_b
            .purge_instance_data()
            .expect("release B's successful acquisition");

        if runtime_a.parent().unwrap().exists() {
            fs::remove_dir_all(runtime_a.parent().unwrap().parent().unwrap())
                .expect("remove shared-claim runtime root");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn partial_acquisitions_and_ambiguous_failed_attempts_stay_journaled() {
        let first_uri = "teststorage://pool/attached";
        let second_uri = "teststorage://pool/ambiguous";
        let runtime_dir = temp_runtime_dir("journal-partial");
        let vm_id = format!("journal-partial-{}", ulid::Ulid::generate());
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let state = Arc::new(Mutex::new(AcquisitionState {
            fail_uri: Some(second_uri.to_owned()),
            ..Default::default()
        }));
        let mut instance = VMInstance::new(
            &vm_id,
            runtime_dir.join(SOCKET_FILE_NAME),
            Some(tracked_storage_transformer(Arc::clone(&state))),
            None,
        );
        instance.runtime_dir_override = Some(runtime_dir.clone());
        let error = instance
            .create_config(storage_config(&[first_uri, second_uri]), false)
            .await
            .expect_err("second attach fails after simulated acquisition");
        assert!(format!("{error:#}").contains("simulated attach failure"));

        assert_eq!(
            VMInstance::load_storage_cleanup_journal(&runtime_dir)
                .expect("read durable journal")
                .iter()
                .map(url::Url::as_str)
                .collect::<Vec<_>>(),
            [first_uri, second_uri]
        );
        instance
            .purge_instance_data()
            .expect("cleanup releases both acquired and ambiguous attempts");
        let state = state.lock().unwrap();
        assert_eq!(state.resolve_attempts, [first_uri, second_uri]);
        assert_eq!(state.release_attempts, [first_uri, second_uri]);
        assert!(state.active.is_empty());
        drop(state);
        if runtime_dir.exists() {
            fs::remove_dir_all(runtime_dir).expect("remove runtime dir");
        }
    }

    #[test]
    fn fake_iscsi_cli_pins_acquired_portal_and_releases_one_session_for_two_luns() {
        let root =
            std::env::temp_dir().join(format!("odorobo-fake-iscsi-cli-{}", ulid::Ulid::generate()));
        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).expect("create fake CLI directory");
        let cli = bin_dir.join("iscsiadm");
        fs::write(
            &cli,
            r#"#!/bin/sh
state="$ODOROBO_FAKE_ISCSI_STATE"
log="$ODOROBO_FAKE_ISCSI_LOG"
if [ "$1" = "-m" ] && [ "$2" = "session" ]; then
    if [ "$3" = "-r" ]; then
        sid="$4"
        printf 'LOGOUT %s\n' "$sid" >> "$log"
        tmp="$state.tmp"
        : > "$tmp"
        while IFS= read -r line; do
            if printf '%s\n' "$line" | grep -Fq "[$sid]"; then
                continue
            fi
            printf '%s\n' "$line" >> "$tmp"
        done < "$state"
        mv "$tmp" "$state"
        exit 0
    fi
    if [ -s "$state" ]; then
        cat "$state"
        exit 0
    fi
    echo 'iscsiadm: No active sessions.' >&2
    exit 21
fi
iqn=""
portal=""
while [ "$#" -gt 0 ]; do
    if [ "$1" = "-T" ]; then
        iqn="$2"
        shift 2
    elif [ "$1" = "-p" ]; then
        portal="$2"
        shift 2
    else
        shift
    fi
done
printf 'LOGIN %s\n' "$iqn" >> "$log"
if [ "$portal" = "localhost:3260" ]; then
    address="127.0.0.1"
else
    address="192.0.2.10"
fi
printf 'tcp: [11] %s:3260,1 %s\n' "$address" "$iqn" >> "$state"
exit 0
"#,
        )
        .expect("write fake iscsiadm CLI");
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o755))
            .expect("make fake iscsiadm executable");
        let state = root.join("sessions.txt");
        let log = root.join("commands.log");
        let path = format!("{}:/usr/bin:/bin", bin_dir.display());
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ch_driver::instance::tests::fake_iscsi_cli_lifecycle_child",
                "--nocapture",
            ])
            .env("PATH", path)
            .env("ODOROBO_FAKE_ISCSI_CHILD", "1")
            .env("ODOROBO_FAKE_ISCSI_STATE", &state)
            .env("ODOROBO_FAKE_ISCSI_LOG", &log)
            .output()
            .expect("run fake-CLI lifecycle child");
        assert!(
            output.status.success(),
            "fake iSCSI lifecycle child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(root).expect("remove fake CLI fixture");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fake_iscsi_cli_lifecycle_child() {
        if std::env::var_os("ODOROBO_FAKE_ISCSI_CHILD").is_none() {
            return;
        }
        let state_path = PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_ISCSI_STATE").expect("fake session state path"),
        );
        let log_path =
            PathBuf::from(std::env::var_os("ODOROBO_FAKE_ISCSI_LOG").expect("fake CLI log path"));
        fs::write(&state_path, "").expect("clear fake sessions");
        fs::write(&log_path, "").expect("clear fake CLI log");

        let runtime_dir = temp_runtime_dir("iscsi-dns-pinning");
        fs::create_dir_all(&runtime_dir).expect("create DNS test runtime dir");
        let vm_id = format!("iscsi-dns-{}", ulid::Ulid::generate());
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).expect("bind mock VMM for DNS test");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept VM create");
            assert_eq!(request_target(&mut stream).await, "/api/v1/vm.create");
            respond(&mut stream, 200, b"{}").await;
        });
        let mut instance = VMInstance::new(
            &vm_id,
            socket,
            Some(TransformChain::new().add(StorageDriverTransformer::default())),
            None,
        );
        instance.runtime_dir_override = Some(runtime_dir.clone());
        let target_a = "iscsi://localhost:3260/iqn.2024-01.example:dns-target/0";
        instance
            .create_config(storage_config(&[target_a]), false)
            .await
            .expect("VM create succeeds with its mock Cloud Hypervisor API");
        server.await.expect("mock VMM create request completes");

        // Model DNS changing from portal A to B while the old acquired session
        // remains. Release must use the persisted numeric session identity.
        fs::write(
            &state_path,
            "tcp: [11] 127.0.0.1:3260,1 iqn.2024-01.example:dns-target\ntcp: [22] 192.0.2.11:3260,1 iqn.2024-01.example:dns-target\n",
        )
        .expect("simulate changed DNS with old and new target sessions");
        instance
            .purge_instance_data()
            .expect("release only the pinned A session");
        let sessions = fs::read_to_string(&state_path).expect("read remaining sessions");
        assert!(sessions.contains("[22] 192.0.2.11:3260"));
        assert!(!sessions.contains("[11]"));
        let commands = fs::read_to_string(&log_path).expect("read fake CLI log");
        assert!(commands.contains("LOGOUT 11"));
        assert!(!commands.contains("LOGOUT 22"));
        drop(instance);
        fs::remove_dir_all(runtime_dir.parent().unwrap().parent().unwrap())
            .expect("remove DNS test runtime root");

        exercise_iscsi_multi_lun(&state_path, &log_path).await;
        exercise_non_iqn_iscsi_targets(&state_path, &log_path).await;
    }

    async fn exercise_iscsi_multi_lun(state_path: &PathBuf, log_path: &PathBuf) {
        fs::write(state_path, "").expect("clear fake sessions for multi-LUN case");
        fs::write(log_path, "").expect("clear fake CLI log for multi-LUN case");
        let runtime_dir = temp_runtime_dir("iscsi-multi-lun");
        fs::create_dir_all(&runtime_dir).expect("create multi-LUN runtime dir");
        let vm_id = format!("iscsi-luns-{}", ulid::Ulid::generate());
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).expect("bind mock VMM for multi-LUN test");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept two-LUN VM create");
            assert_eq!(request_target(&mut stream).await, "/api/v1/vm.create");
            respond(&mut stream, 200, b"{}").await;
        });
        let mut instance = VMInstance::new(
            &vm_id,
            socket,
            Some(TransformChain::new().add(StorageDriverTransformer::default())),
            None,
        );
        instance.runtime_dir_override = Some(runtime_dir.clone());
        let lun0 = "iscsi://192.0.2.10:3260/iqn.2024-01.example:multi-lun/0";
        let lun1 = "iscsi://192.0.2.10:3260/iqn.2024-01.example:multi-lun/1";
        instance
            .create_config(storage_config(&[lun0, lun1]), false)
            .await
            .expect("two-LUN VM create succeeds");
        server.await.expect("mock VMM accepted two-LUN create");
        assert_eq!(
            instance
                .vm_config
                .as_ref()
                .unwrap()
                .disks
                .as_ref()
                .unwrap()
                .len(),
            2
        );
        let commands = fs::read_to_string(log_path).expect("read multi-LUN CLI log");
        assert_eq!(commands.matches("LOGIN ").count(), 1);
        assert_eq!(fs::read_to_string(state_path).unwrap().lines().count(), 1);
        instance
            .purge_instance_data()
            .expect("release the shared iSCSI session once");
        let commands = fs::read_to_string(log_path).expect("read multi-LUN release log");
        assert_eq!(commands.matches("LOGIN ").count(), 1);
        assert_eq!(commands.matches("LOGOUT 11").count(), 1);
        assert!(fs::read_to_string(state_path).unwrap().is_empty());
        drop(instance);
        fs::remove_dir_all(runtime_dir.parent().unwrap().parent().unwrap())
            .expect("remove multi-LUN test runtime root");
    }

    async fn exercise_non_iqn_iscsi_targets(state_path: &PathBuf, log_path: &PathBuf) {
        // Non-IQN iSCSI names must acquire and persist the same pinned session
        // identity, and a restart with unrecognized session output must retain
        // cleanup metadata rather than interpreting the session as absent.
        for target_name in ["eui.0011223344556677", "naa.6001405abc123456"] {
            fs::write(state_path, "").expect("clear sessions for target-name case");
            fs::write(log_path, "").expect("clear CLI log for target-name case");
            let runtime_dir = temp_runtime_dir("iscsi-target-name-restart");
            fs::create_dir_all(&runtime_dir).expect("create target-name runtime dir");
            let vm_id = format!("iscsi-target-name-{}", ulid::Ulid::generate());
            let socket = runtime_dir.join(SOCKET_FILE_NAME);
            let listener = UnixListener::bind(&socket).expect("bind mock CH for target-name case");
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept target-name create");
                assert_eq!(request_target(&mut stream).await, "/api/v1/vm.create");
                respond(&mut stream, 200, b"{}").await;
            });
            let mut instance = VMInstance::new(
                &vm_id,
                socket,
                Some(TransformChain::new().add(StorageDriverTransformer::default())),
                None,
            );
            instance.runtime_dir_override = Some(runtime_dir.clone());
            let target_uri = format!("iscsi://192.0.2.10:3260/{target_name}/0");
            instance
                .create_config(storage_config(&[&target_uri]), false)
                .await
                .expect("acquire EUI/NAA target session");
            server.await.expect("mock VMM create completes");

            let claims_dir = runtime_dir
                .parent()
                .unwrap()
                .join(super::STORAGE_CLAIMS_DIR_NAME);
            let claim = fs::read_dir(&claims_dir)
                .expect("read durable target claim")
                .filter_map(std::result::Result::ok)
                .find(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .expect("durable target claim exists");
            let claim_json: serde_json::Value = serde_json::from_reader(
                fs::File::open(claim.path()).expect("open durable target claim"),
            )
            .expect("parse durable target claim");
            assert!(
                claim_json["attachment"].as_str().is_some(),
                "{target_name} acquired session identity must be persisted"
            );
            drop(instance);

            fs::write(state_path, "unrecognized session output\n")
                .expect("simulate uncertain session listing after restart");
            let error = VMInstance::cleanup_persisted_configs(
                &vm_id,
                &runtime_dir,
                &TransformChain::new().add(StorageDriverTransformer::default()),
            )
            .expect_err("unknown session row must keep cleanup uncertain");
            assert!(format!("{error:#}").contains("Malformed iSCSI session row"));
            assert!(
                VMInstance::load_storage_cleanup_journal(&runtime_dir)
                    .expect("read retained cleanup journal")
                    .iter()
                    .any(|uri| uri.as_str() == target_uri),
                "uncertain cleanup must retain the journal entry"
            );
            let uncertain_claim: serde_json::Value = serde_json::from_reader(
                fs::File::open(claim.path()).expect("reopen uncertain storage claim"),
            )
            .expect("parse uncertain storage claim");
            assert_eq!(
                uncertain_claim["release_started"],
                serde_json::Value::Bool(true),
                "release intent must be durable before backend release is attempted"
            );

            fs::write(
                state_path,
                format!("tcp: [11] 192.0.2.10:3260,1 {target_name}\n"),
            )
            .expect("restore valid EUI/NAA session output");
            let error = VMInstance::cleanup_persisted_configs(
                &vm_id,
                &runtime_dir,
                &TransformChain::new().add(StorageDriverTransformer::default()),
            )
            .expect_err("an earlier release attempt leaves logout uncertain");
            assert!(format!("{error:#}").contains("release is uncertain"));
            assert!(
                fs::read_to_string(state_path)
                    .expect("read retained session")
                    .contains("[11]"),
                "an uncertain release must not retry logout"
            );
            assert!(
                !fs::read_to_string(log_path)
                    .expect("read target-name CLI log")
                    .contains("LOGOUT 11")
            );
            fs::remove_dir_all(runtime_dir.parent().unwrap().parent().unwrap())
                .expect("remove target-name runtime root");
        }
    }

    #[tokio::test]
    async fn reattach_restores_transformed_vm_config_from_cloud_hypervisor() {
        let runtime_dir = temp_runtime_dir("reattach-config");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).expect("bind mock CH socket");
        let expected_config = transformed_disk_config();
        let info = VmInfo::new(expected_config.clone(), VmState::Running);
        let info_body = serde_json::to_vec(&info).expect("serialize VM info");
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept CH request");
                match request_target(&mut stream).await.as_str() {
                    "/api/v1/vmm.ping" => {
                        respond(&mut stream, 200, br#"{"version":"mock"}"#).await;
                    }
                    "/api/v1/vm.info" => respond(&mut stream, 200, &info_body).await,
                    target => panic!("unexpected CH request target: {target}"),
                }
            }
        });

        assert!(
            VMInstance::prepare_runtime_dir_for_spawn(&runtime_dir)
                .await
                .expect("probe existing VMM")
        );
        let instance = VMInstance::reattach(
            "reattach-test",
            socket,
            Some(VmConfig::default()),
            false,
            Some(TransformChain::new()),
        )
        .await
        .expect("reattach to configured VMM");
        let actual = instance.vm_config.as_ref().expect("restored config");
        let disk = actual
            .disks
            .as_ref()
            .expect("restored disks")
            .first()
            .unwrap();
        assert_eq!(disk.path.as_deref(), Some("/dev/rbd/pool/image"));
        assert_eq!(
            disk.id.as_deref(),
            Some("rbd://pool/image?id=original-disk-id")
        );
        server.await.expect("mock CH server completes");
        fs::remove_dir_all(runtime_dir).expect("remove runtime dir");
    }

    #[tokio::test]
    async fn empty_responsive_vmm_is_initialized_and_booted_on_reattach() {
        let runtime_dir = temp_runtime_dir("empty-vmm");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).expect("bind mock CH socket");
        let info_body = serde_json::to_vec(&VmInfo::new(VmConfig::default(), VmState::Created))
            .expect("serialize VM info");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            let mut info_calls = 0;
            for _ in 0..6 {
                let (mut stream, _) = listener.accept().await.expect("accept CH request");
                let target = request_target(&mut stream).await;
                observed.lock().unwrap().push(target.clone());
                match target.as_str() {
                    "/api/v1/vmm.ping" => {
                        respond(&mut stream, 200, br#"{"version":"mock"}"#).await;
                    }
                    "/api/v1/vm.create" | "/api/v1/vm.boot" => {
                        respond(&mut stream, 200, b"{}").await;
                    }
                    "/api/v1/vm.info" => {
                        info_calls += 1;
                        if info_calls == 1 {
                            respond(&mut stream, 404, br#"{"description":"no VM"}"#).await;
                        } else {
                            respond(&mut stream, 200, &info_body).await;
                        }
                    }
                    unexpected => panic!("unexpected CH request: {unexpected}"),
                }
            }
        });

        assert!(
            VMInstance::prepare_runtime_dir_for_spawn(&runtime_dir)
                .await
                .expect("probe bare VMM")
        );
        let vm_id = ulid::Ulid::generate().to_string();
        let instance = VMInstance::reattach(
            &vm_id,
            socket,
            Some(VmConfig::default()),
            true,
            Some(TransformChain::new()),
        )
        .await
        .expect("recover empty VMM");
        assert!(instance.vm_config.is_some(), "config should be retained");
        server.await.expect("mock CH server completes");
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| *r == "/api/v1/vm.create")
                .count(),
            1
        );
        assert_eq!(
            requests.iter().filter(|r| *r == "/api/v1/vm.boot").count(),
            1
        );
        fs::remove_dir_all(runtime_dir).expect("remove runtime dir");
    }

    async fn assert_reattach_boot_intent(state: VmState, boot: bool, expected_boots: usize) {
        let runtime_dir = temp_runtime_dir("reattach-boot-intent");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).expect("bind mock CH socket");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&requests);
        let expected_requests = if expected_boots == 0 { 1 } else { 4 };
        let server = tokio::spawn(async move {
            let mut booted = false;
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().await.expect("accept CH request");
                let target = request_target(&mut stream).await;
                observed.lock().unwrap().push(target.clone());
                match target.as_str() {
                    "/api/v1/vm.info" => {
                        let reported_state = if booted { VmState::Running } else { state };
                        let info = VmInfo::new(VmConfig::default(), reported_state);
                        let body = serde_json::to_vec(&info).expect("serialize VM info");
                        respond(&mut stream, 200, &body).await;
                    }
                    "/api/v1/vm.boot" => {
                        booted = true;
                        respond(&mut stream, 200, b"{}").await;
                    }
                    target => panic!("unexpected CH request: {target}"),
                }
            }
        });

        let vm_id = ulid::Ulid::generate().to_string();
        let instance = VMInstance::reattach(
            &vm_id,
            socket,
            Some(VmConfig::default()),
            boot,
            Some(TransformChain::new()),
        )
        .await
        .expect("reattach to configured VMM");
        assert!(
            instance.vm_config.is_some(),
            "reattached config is retained"
        );
        server.await.expect("mock CH server completes");
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.iter().filter(|r| *r == "/api/v1/vm.boot").count(),
            expected_boots,
            "state {state:?} with boot={boot} should issue the expected boot calls"
        );
        fs::remove_dir_all(runtime_dir).expect("remove runtime dir");
    }

    #[tokio::test]
    async fn reattach_honors_boot_intent_only_for_created_vms() {
        assert_reattach_boot_intent(VmState::Created, true, 1).await;
        assert_reattach_boot_intent(VmState::Created, false, 0).await;
        assert_reattach_boot_intent(VmState::Running, true, 0).await;
        assert_reattach_boot_intent(VmState::Paused, true, 0).await;
    }

    #[tokio::test]
    async fn nonresponding_socket_probe_is_bounded_and_preserves_runtime_files() {
        let runtime_dir = temp_runtime_dir("unresponsive-probe");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).expect("bind mock CH socket");
        for name in [
            CONSOLE_SOCKET_FILE_NAME,
            SOCKET_LOCK_FILE_NAME,
            CONFIG_FILE_NAME,
        ] {
            fs::write(runtime_dir.join(name), b"preserve").expect("create runtime marker");
        }
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept probe");
            let _hold_open = stream;
            std::future::pending::<()>().await;
        });

        let start = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_millis(300),
            VMInstance::prepare_runtime_dir_for_spawn_with_timeout(
                &runtime_dir,
                Duration::from_millis(50),
            ),
        )
        .await
        .expect("probe must be bounded");
        assert!(outcome.is_err(), "unresponsive socket must be uncertain");
        assert!(start.elapsed() < Duration::from_millis(300));
        assert!(
            socket.exists(),
            "uncertain live socket must not be unlinked"
        );
        for name in [
            CONSOLE_SOCKET_FILE_NAME,
            SOCKET_LOCK_FILE_NAME,
            CONFIG_FILE_NAME,
        ] {
            assert!(runtime_dir.join(name).exists(), "{name} must be preserved");
        }
        server.abort();
        let join_error = server.await.expect_err("aborted mock server should stop");
        assert!(join_error.is_cancelled());
        fs::remove_dir_all(runtime_dir).expect("remove runtime dir");
    }

    #[tokio::test]
    async fn destroy_does_not_report_success_or_drop_config_for_uncertain_vmm() {
        let runtime_dir = temp_runtime_dir("uncertain-destroy");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).expect("bind mock CH socket");
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept CH request");
                tokio::spawn(async move {
                    let _hold_open = stream;
                    std::future::pending::<()>().await;
                });
            }
        });
        let mut instance = VMInstance::new(
            "uncertain-destroy",
            socket,
            Some(TransformChain::new()),
            None,
        );
        instance.runtime_dir_override = Some(runtime_dir.clone());
        instance.vm_config = Some(transformed_disk_config());

        let result = instance.destroy().await;
        assert!(result.is_err(), "uncertain VMM teardown must fail");
        assert!(instance.vm_config.is_some(), "config must remain for retry");
        assert!(runtime_dir.exists(), "runtime state must remain for retry");
        server.abort();
        let join_error = server.await.expect_err("aborted mock server should stop");
        assert!(join_error.is_cancelled());
        fs::remove_dir_all(runtime_dir).expect("remove runtime dir");
    }

    #[tokio::test]
    async fn destroy_cleans_up_a_stale_dead_vmm_socket() {
        let runtime_dir = temp_runtime_dir("dead-vmm");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).expect("bind stale CH socket");
        drop(listener);
        let mut instance = VMInstance::new("dead-vmm", socket, Some(TransformChain::new()), None);
        instance.runtime_dir_override = Some(runtime_dir.clone());
        instance.vm_config = Some(transformed_disk_config());

        instance
            .destroy()
            .await
            .expect("dead VMM cleanup should succeed");
        assert!(!runtime_dir.exists(), "dead VMM runtime should be removed");
    }

    struct TransformThenFail(Arc<std::sync::atomic::AtomicBool>);

    impl ConfigTransform for TransformThenFail {
        fn transform(&self, _vmid: &str, config: &mut VmConfig) -> Result<()> {
            config.disks = Some(vec![DiskConfig {
                path: Some("/runtime/acquired-resource".to_owned()),
                id: Some("test-resource-marker".to_owned()),
                ..Default::default()
            }]);
            Err(eyre!("simulated later transform failure"))
        }

        fn teardown(&self, _vmid: &str, config: &mut VmConfig) -> Result<()> {
            assert_eq!(
                config.disks.as_ref().unwrap()[0].id.as_deref(),
                Some("test-resource-marker")
            );
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn partial_transform_is_retained_for_failed_start_cleanup() {
        let runtime_dir = temp_runtime_dir("partial-transform");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let teardown_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut instance = VMInstance::new(
            "partial-transform",
            runtime_dir.join(SOCKET_FILE_NAME),
            Some(TransformChain::new().add(TransformThenFail(Arc::clone(&teardown_called)))),
            None,
        );
        instance.runtime_dir_override = Some(runtime_dir.clone());

        assert!(
            instance
                .create_config(VmConfig::default(), false)
                .await
                .is_err()
        );
        assert!(instance.vm_config.is_some());
        instance
            .purge_instance_data()
            .expect("teardown partial transforms");
        assert!(teardown_called.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!runtime_dir.exists());
    }

    struct FailingTeardown;

    impl ConfigTransform for FailingTeardown {
        fn transform(&self, _vmid: &str, _config: &mut VmConfig) -> Result<()> {
            Ok(())
        }

        fn teardown(&self, _vmid: &str, _config: &mut VmConfig) -> Result<()> {
            Err(eyre!("simulated transform teardown failure"))
        }
    }

    #[tokio::test]
    async fn teardown_failure_is_reported_and_retains_vm_config_for_retry() {
        let runtime_dir = temp_runtime_dir("teardown-failure");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let mut instance = VMInstance::new(
            "teardown-failure",
            runtime_dir.join(SOCKET_FILE_NAME),
            Some(TransformChain::new().add(FailingTeardown)),
            None,
        );
        instance.runtime_dir_override = Some(runtime_dir.clone());
        instance.vm_config = Some(transformed_disk_config());

        assert!(instance.destroy().await.is_err());
        assert!(
            instance.vm_config.is_some(),
            "failed teardown config must be retained"
        );
        fs::remove_dir_all(runtime_dir).expect("remove runtime dir");
    }

    #[tokio::test]
    async fn destroy_cleans_stale_socket_after_owned_child_exits() {
        let runtime_dir = temp_runtime_dir("owned-exited");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).expect("bind stale CH socket");
        drop(listener);
        let child = Command::new("sleep")
            .arg("0.01")
            .spawn()
            .expect("spawn short-lived test child");
        let mut instance = VMInstance::new(
            "owned-exited",
            socket,
            Some(TransformChain::new()),
            Some(child),
        );
        instance.runtime_dir_override = Some(runtime_dir.clone());
        instance.vm_config = Some(transformed_disk_config());
        let (child, watcher) = instance
            .take_child_process_for_watcher()
            .expect("transfer child to watcher");
        let (status, stopped_for_teardown) = watcher.wait(child).await;
        assert!(status.expect("child wait succeeds").success());
        assert!(!stopped_for_teardown);

        instance
            .destroy()
            .await
            .expect("clean up after owned child exit");
        assert!(!runtime_dir.exists(), "stale runtime must be purged");
    }

    #[tokio::test]
    async fn destroy_kills_and_reaps_an_unresponsive_owned_watched_process() {
        let runtime_dir = temp_runtime_dir("owned-unresponsive");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        let socket = runtime_dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).expect("bind mock CH socket");
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept CH request");
                tokio::spawn(async move {
                    let _hold_open = stream;
                    std::future::pending::<()>().await;
                });
            }
        });
        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn test child process");
        let mut instance = VMInstance::new(
            "owned-unresponsive",
            socket,
            Some(TransformChain::new()),
            Some(child),
        );
        instance.runtime_dir_override = Some(runtime_dir.clone());
        instance.vm_config = Some(transformed_disk_config());
        let (child, watcher) = instance
            .take_child_process_for_watcher()
            .expect("transfer child to watcher");
        let watcher = tokio::spawn(watcher.wait(child));

        instance
            .destroy()
            .await
            .expect("destroy owned unresponsive VMM");
        let (status, stopped_for_teardown) = watcher.await.expect("watcher completes");
        assert!(!status.expect("child wait succeeds").success());
        assert!(
            stopped_for_teardown,
            "destroy should request watcher termination"
        );
        assert!(!runtime_dir.exists(), "owned VMM runtime should be removed");
        server.abort();
        let join_error = server.await.expect_err("aborted mock server should stop");
        assert!(join_error.is_cancelled());
    }
}
