use crate::ch_driver::transform::{
    ConfigTransform, StorageClaim, StorageCleanupContext, StorageOwnershipContext,
    StorageReleaseState,
};
use async_trait::async_trait;
use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::{
    Result,
    eyre::{WrapErr, eyre},
};
use std::{
    path::PathBuf,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};
use tracing::warn;
use url::Url;

const STORAGE_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const STORAGE_COMMAND_REAP_TIMEOUT: Duration = Duration::from_secs(5);

mod file;
mod iscsi;
mod rbd;

/// A storage backend for Odorobo to resolve storage URIs to local paths.
///
/// This allows Odorobo to actually convert a custom URI
/// scheme to a local block device or file path that can be mapped to a VM,
/// and allow them to be released when the VM is stopped.
///
/// For example, a storage backend could resolve `rdb://pool/disk1` to `/run/odorobo/devices/pool/disk1`
/// and create a symlink to `/dev/rdbN` there, returning that path
/// when `resolve` is called. When `release` is called, the symlink and the resolved path can be cleaned up.
///
/// This helps orchestrators to deal with storage management, by offloading the responsibility of attaching
/// LUNs to the agent, and letting the orchestrator just tell the agent to resolve a URI to a path, and release it when done.
///
/// Additional metadata may also be stored in the storage backend, such as the original URI, to allow for other agent
/// instances to resolve the same URI properly, or for debugging purposes. This is up to the implementation of the storage backend.

// using async_trait here because even with Rust 1.75, dyn in async traits do not work due to
// vtable issues
#[async_trait]
pub trait StorageDriver: Send + Sync {
    /// The URI scheme this backend handles, e.g. `"rbd"`, `"file"`. Used for dispatch in `StorageChain`.
    fn scheme(&self) -> &'static str;

    /// Normalized node-local resource identity for exclusive attachments.
    /// Drivers that do not attach an exclusive kernel resource may return None.
    fn ownership_key(&self, uri: &Url) -> Result<Option<String>> {
        Ok(Some(format!("{}:{}", self.scheme(), uri.as_str())))
    }

    /// Check that this driver can acquire the resource without reusing a
    /// resource owned by another VM. Drivers with exclusive resources should
    /// override this and reject already-active attachments.
    async fn ensure_exclusive(&self, _uri: &Url) -> Result<()> {
        Ok(())
    }

    /// Resolves a URI to a local block device or file path for use in a VM disk config.
    async fn resolve(&self, uri: &Url) -> Result<PathBuf>;

    /// Resolve a newly claimed resource, persisting backend-specific attachment
    /// identity before returning it to the VM. Existing same-VM claims are
    /// passed through `existing_attachment` so a backend can share one session
    /// across multiple disks without acquiring it again.
    async fn resolve_claimed(
        &self,
        uri: &Url,
        _is_new: bool,
        _existing_attachment: Option<&str>,
        _persist_attachment: &mut (dyn for<'a> FnMut(&'a str) -> Result<()> + Send),
    ) -> Result<PathBuf> {
        self.resolve(uri).await
    }

    /// Releases resources associated with a previously resolved URI.
    async fn release(&self, uri: &Url) -> Result<()>;

    /// Release using the exact pinned attachment identity, when a backend has
    /// one. Legacy backends continue to release by URI.
    async fn release_claimed(&self, uri: &Url, _attachment: Option<&str>) -> Result<()> {
        self.release(uri).await
    }

    /// Release after the durable cleanup intent has been recorded. A retry is
    /// distinguished from the initial attempt so backends can conservatively
    /// avoid acting on a kernel name that may have been recycled after a crash.
    async fn release_claimed_with_intent(
        &self,
        uri: &Url,
        attachment: Option<&str>,
        _release_state: StorageReleaseState,
    ) -> Result<()> {
        self.release_claimed(uri, attachment).await
    }
}

/// Execute a storage CLI with a deadline and ensure a timed-out child is killed
/// and reaped before returning to the caller.
pub(super) async fn run_storage_command(
    command: &mut Command,
    description: &str,
) -> Result<Output> {
    run_storage_command_with_timeout(command, description, STORAGE_COMMAND_TIMEOUT).await
}

async fn run_storage_command_with_timeout(
    command: &mut Command,
    description: &str,
    timeout: Duration,
) -> Result<Output> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| eyre!("Failed to execute {description}: {error}"))?;
    let pid = child.id();
    let mut stdout_reader = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("Failed to capture stdout from {description}"))?;
    let mut stderr_reader = child
        .stderr
        .take()
        .ok_or_else(|| eyre!("Failed to capture stderr from {description}"))?;

    let output_future = async {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let (stdout_result, stderr_result, status_result) = tokio::join!(
            stdout_reader.read_to_end(&mut stdout),
            stderr_reader.read_to_end(&mut stderr),
            child.wait()
        );
        stdout_result.map_err(|error| eyre!("Failed to read {description} stdout: {error}"))?;
        stderr_result.map_err(|error| eyre!("Failed to read {description} stderr: {error}"))?;
        let status = status_result
            .map_err(|error| eyre!("Failed while waiting for {description}: {error}"))?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    };

    if let Ok(result) = tokio::time::timeout(timeout, output_future).await {
        result
    } else {
        let kill_error = child.start_kill().err();
        match tokio::time::timeout(STORAGE_COMMAND_REAP_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) => Err(eyre!(
                "Timed out {description} after {timeout:?}; child {pid:?} was reaped with {status}{}",
                kill_error.map_or_else(String::new, |error| format!(" (kill request: {error})"))
            )),
            Ok(Err(error)) => Err(eyre!(
                "Timed out {description} after {timeout:?}; failed to reap child {pid:?}: {error}"
            )),
            Err(_) => Err(eyre!(
                "Timed out {description} after {timeout:?}; child {pid:?} did not exit after kill"
            )),
        }
    }
}

/// A chain of storage backends that dispatches disk URI resolution to the backend
/// whose scheme matches the URI scheme.
///
/// Disk paths that are not URIs or whose scheme has no registered backend are left unchanged.
pub struct StorageDriverTransformer {
    backends: Vec<Box<dyn StorageDriver>>,
}

impl StorageDriverTransformer {
    pub fn new() -> Self {
        Self { backends: vec![] }
    }

    pub fn with_backend<B: StorageDriver + 'static>(mut self, backend: B) -> Self {
        self.backends.push(Box::new(backend));
        self
    }

    fn find_backend(&self, uri: &Url) -> Option<&dyn StorageDriver> {
        self.backends
            .iter()
            .find(|b| b.scheme() == uri.scheme())
            .map(std::convert::AsRef::as_ref)
    }

    /// Releases storage resources identified by the acquired-resource journal.
    /// This deliberately does not infer ownership from desired VM config disks.
    pub async fn release_journal(&self, journal: &[Url]) -> Result<()> {
        let mut failures = Vec::new();
        for uri in journal {
            let Some(backend) = self.find_backend(uri) else {
                failures.push(format!("{uri}: no storage backend registered"));
                continue;
            };
            if let Err(error) = backend.release(uri).await {
                warn!(path = uri.as_str(), error = ?error, "Failed to release journaled storage");
                failures.push(format!("{uri}: {error:#}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(eyre!(
                "Failed to release storage for {} journal entry/entries: {}",
                failures.len(),
                failures.join("; ")
            ))
        }
    }

    /// Legacy helper for direct callers. Instance cleanup uses the durable
    /// acquired-resource journal instead of inferring ownership from config.
    pub async fn release_config(&self, config: &VmConfig) -> Result<()> {
        let Some(disks) = config.disks.as_ref() else {
            return Ok(());
        };
        let mut uris = Vec::new();
        for disk in disks {
            let candidates = [disk.path.as_deref(), disk.id.as_deref()];
            if let Some(uri) = candidates.into_iter().flatten().find_map(|value| {
                let uri = Url::parse(value).ok()?;
                self.find_backend(&uri).map(|_| uri)
            }) {
                uris.push(uri);
            }
        }
        self.release_journal(&uris).await
    }

    fn transform_disks(
        &self,
        config: &mut VmConfig,
        record: &mut dyn FnMut(&Url) -> Result<()>,
    ) -> Result<()> {
        let Some(disks) = config.disks.as_mut() else {
            return Ok(());
        };
        for disk in disks {
            let Some(path) = disk.path.as_deref() else {
                continue;
            };
            let Ok(uri) = Url::parse(path) else {
                continue;
            };
            let Some(backend) = self.find_backend(&uri) else {
                warn!(
                    scheme = uri.scheme(),
                    path,
                    "No storage backend registered for URI scheme, leaving disk path unchanged"
                );
                continue;
            };

            // Reject a known external attachment before putting it in this
            // VM's cleanup journal. Then persist cleanup eligibility before
            // resolve: a backend may acquire its resource and then fail or the
            // process may crash before returning the resolved path.
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(backend.ensure_exclusive(&uri))
            })?;
            record(&uri)?;
            let new_disk_id = uri
                .clone()
                .query_pairs_mut()
                .append_pair("id", disk.id.as_deref().unwrap_or("<unknown>"))
                .finish()
                .to_string();
            let resolved = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(backend.resolve(&uri))
            })?;
            disk.id = Some(new_disk_id);
            disk.path = Some(resolved.to_string_lossy().into_owned());
        }
        Ok(())
    }

    fn transform_disks_with_ownership(
        &self,
        config: &mut VmConfig,
        ownership: &mut StorageOwnershipContext<'_>,
    ) -> Result<()> {
        let Some(disks) = config.disks.as_mut() else {
            return Ok(());
        };
        for disk in disks {
            let Some(path) = disk.path.as_deref() else {
                continue;
            };
            let Ok(uri) = Url::parse(path) else {
                continue;
            };
            let Some(backend) = self.find_backend(&uri) else {
                continue;
            };
            let resource_key = backend.ownership_key(&uri)?;
            let claim = match resource_key.as_deref() {
                Some(key) => (ownership.claim)(&uri, key)?,
                None => StorageClaim {
                    is_new: true,
                    journaled: false,
                    attachment: None,
                    recovered: false,
                    release_started: false,
                },
            };

            if claim.release_started {
                return Err(eyre!(
                    "Storage resource {uri} is already in an uncertain release phase"
                ));
            }

            if claim.is_new || !claim.journaled {
                let precheck = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(backend.ensure_exclusive(&uri))
                });
                if let Err(error) = precheck {
                    if let Some(key) = resource_key.as_deref()
                        && let Err(abandon_error) = (ownership.abandon)(&uri, key)
                    {
                        return Err(eyre!(
                            "Storage precheck failed ({error:#}) and ownership claim cleanup failed ({abandon_error:#})"
                        ));
                    }
                    return Err(error);
                }
            }
            // The VM journal becomes durable only after the known-rejection
            // precheck, but before any backend operation that can acquire. A
            // same-VM claim already journaled for another LUN shares its one
            // backend attachment and therefore needs no duplicate entry.
            if resource_key.is_none() || !claim.journaled {
                if let Err(error) = (ownership.record)(&uri) {
                    return Err(error).wrap_err("Failed to persist storage acquisition attempt");
                }
                if let Some(key) = resource_key.as_deref() {
                    (ownership.mark_journaled)(&uri, key)?;
                }
            }

            let new_disk_id = uri
                .clone()
                .query_pairs_mut()
                .append_pair("id", disk.id.as_deref().unwrap_or("<unknown>"))
                .finish()
                .to_string();
            let key = resource_key.as_deref();
            let mut persist_attachment = |attachment: &str| match key {
                Some(key) => (ownership.set_attachment)(&uri, key, attachment),
                None => Ok(()),
            };
            let resolved = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(backend.resolve_claimed(
                    &uri,
                    claim.is_new,
                    claim.attachment.as_deref(),
                    &mut persist_attachment,
                ))
            })?;
            disk.id = Some(new_disk_id);
            disk.path = Some(resolved.to_string_lossy().into_owned());
        }
        Ok(())
    }
}

impl Default for StorageDriverTransformer {
    fn default() -> Self {
        Self::new()
            .with_backend(file::FileStorage)
            .with_backend(rbd::RbdStorage)
            .with_backend(iscsi::ISCSIStorage)
    }
}

impl ConfigTransform for StorageDriverTransformer {
    fn teardown(&self, _vmid: &str, config: &mut VmConfig) -> Result<()> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.release_config(config))
        })
    }

    fn teardown_with_cleanup_journal(
        &self,
        _vmid: &str,
        _config: &mut VmConfig,
        journal: &[Url],
        forget: &mut dyn FnMut(&Url) -> Result<()>,
    ) -> Result<()> {
        let mut failures = Vec::new();
        for uri in journal {
            let Some(backend) = self.find_backend(uri) else {
                failures.push(format!("{uri}: no storage backend registered"));
                continue;
            };
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(backend.release(uri))
            });
            match result {
                Ok(()) => {
                    if let Err(error) = forget(uri) {
                        failures.push(format!(
                            "{uri}: release succeeded but journal update failed: {error:#}"
                        ));
                    }
                }
                Err(error) => failures.push(format!("{uri}: {error:#}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(eyre!(
                "Failed to release journaled storage: {}",
                failures.join("; ")
            ))
        }
    }

    fn transform(&self, _vmid: &str, config: &mut VmConfig) -> Result<()> {
        self.transform_disks(config, &mut |_| Ok(()))
    }

    fn transform_with_cleanup_journal(
        &self,
        _vmid: &str,
        config: &mut VmConfig,
        record: &mut dyn FnMut(&Url) -> Result<()>,
    ) -> Result<()> {
        self.transform_disks(config, record)
    }

    fn transform_with_storage_ownership(
        &self,
        _vmid: &str,
        config: &mut VmConfig,
        ownership: &mut StorageOwnershipContext<'_>,
    ) -> Result<()> {
        self.transform_disks_with_ownership(config, ownership)
    }

    fn validate_storage_cleanup_metadata(
        &self,
        _vmid: &str,
        config: &VmConfig,
        journal_is_durable: bool,
    ) -> Result<()> {
        if journal_is_durable {
            return Ok(());
        }
        let Some(disks) = config.disks.as_ref() else {
            return Ok(());
        };
        for disk in disks {
            for value in [disk.path.as_deref(), disk.id.as_deref()]
                .into_iter()
                .flatten()
            {
                let Ok(uri) = Url::parse(value) else {
                    continue;
                };
                if self.find_backend(&uri).is_some() {
                    return Err(eyre!(
                        "Storage cleanup journal is missing for configured resource {uri}; retaining VM metadata for manual validation"
                    ));
                }
            }
        }
        Ok(())
    }

    fn teardown_with_storage_ownership(
        &self,
        _vmid: &str,
        _config: &mut VmConfig,
        journal: &[Url],
        cleanup: &mut StorageCleanupContext<'_>,
    ) -> Result<()> {
        let mut failures = Vec::new();
        for uri in journal {
            let Some(backend) = self.find_backend(uri) else {
                failures.push(format!("{uri}: no storage backend registered"));
                continue;
            };
            let key = match backend.ownership_key(uri) {
                Ok(key) => key,
                Err(error) => {
                    failures.push(format!("{uri}: {error:#}"));
                    continue;
                }
            };
            let attachment = match key.as_deref() {
                Some(key) => match (cleanup.attachment)(uri, key) {
                    Ok(attachment) => attachment,
                    Err(error) => {
                        failures.push(format!("{uri}: failed to read attachment claim: {error:#}"));
                        continue;
                    }
                },
                None => None,
            };
            let release_state = match key.as_deref() {
                Some(key) => match (cleanup.begin_release)(uri, key) {
                    Ok(release_state) => release_state,
                    Err(error) => {
                        failures.push(format!(
                            "{uri}: failed to persist release intent: {error:#}"
                        ));
                        continue;
                    }
                },
                None => StorageReleaseState::default(),
            };
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(backend.release_claimed_with_intent(
                    uri,
                    attachment.as_deref(),
                    release_state,
                ))
            });
            match result {
                Ok(()) => {
                    let forgotten = match key.as_deref() {
                        Some(key) => (cleanup.forget)(uri, key),
                        None => (cleanup.forget)(uri, ""),
                    };
                    if let Err(error) = forgotten {
                        failures.push(format!(
                            "{uri}: release succeeded but durable ownership update failed: {error:#}"
                        ));
                    }
                }
                Err(error) => failures.push(format!("{uri}: {error:#}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(eyre!(
                "Failed to release claimed storage: {}",
                failures.join("; ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_hypervisor_client::models::DiskConfig;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeState {
        attempts: Vec<String>,
        active: HashSet<String>,
        fail_once: HashSet<String>,
    }

    #[derive(Clone)]
    struct FakeStorage(Arc<Mutex<FakeState>>);

    #[async_trait]
    impl StorageDriver for FakeStorage {
        fn scheme(&self) -> &'static str {
            "teststorage"
        }

        async fn resolve(&self, _uri: &Url) -> Result<PathBuf> {
            Ok(PathBuf::from("/dev/test-storage"))
        }

        async fn release(&self, uri: &Url) -> Result<()> {
            let resource = uri.host_str().unwrap_or_default().to_owned();
            let mut state = self.0.lock().unwrap();
            state.attempts.push(resource.clone());
            if state.fail_once.remove(&resource) {
                drop(state);
                return Err(eyre!("simulated release failure for {resource}"));
            }
            state.active.remove(&resource);
            drop(state);
            Ok(())
        }
    }

    struct TimeoutStorage(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl StorageDriver for TimeoutStorage {
        fn scheme(&self) -> &'static str {
            "timeoutstorage"
        }

        async fn resolve(&self, _uri: &Url) -> Result<PathBuf> {
            Ok(PathBuf::from("/dev/timeout-storage"))
        }

        async fn release(&self, uri: &Url) -> Result<()> {
            if uri.host_str() == Some("hang") {
                let executable = std::env::current_exe()
                    .map_err(|error| eyre!("Failed to locate test executable: {error}"))?;
                let mut command = Command::new(executable);
                command
                    .args([
                        "--exact",
                        "ch_driver::transform::storage::tests::fake_hanging_command_child",
                        "--nocapture",
                    ])
                    .env("ODOROBO_FAKE_HANGING_STORAGE_COMMAND", "1");
                return run_storage_command_with_timeout(
                    &mut command,
                    "fake hanging storage command",
                    Duration::from_millis(200),
                )
                .await
                .map(|_| ());
            }
            let mut released = self.0.lock().unwrap();
            released.push(uri.host_str().unwrap_or_default().to_owned());
            drop(released);
            Ok(())
        }
    }

    #[tokio::test]
    async fn fake_hanging_command_child() {
        if std::env::var_os("ODOROBO_FAKE_HANGING_STORAGE_COMMAND").is_some() {
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timed_out_storage_command_is_reaped_and_later_releases_continue() {
        let released = Arc::new(Mutex::new(Vec::new()));
        let transformer =
            StorageDriverTransformer::new().with_backend(TimeoutStorage(Arc::clone(&released)));
        let journal = [
            Url::parse("timeoutstorage://hang/disk").unwrap(),
            Url::parse("timeoutstorage://continue/disk").unwrap(),
        ];
        let mut config = VmConfig::default();
        let mut forget = |_uri: &Url| Ok(());
        let started = tokio::time::Instant::now();
        let error = transformer
            .teardown_with_cleanup_journal("test-vm", &mut config, &journal, &mut forget)
            .expect_err("hung storage command should time out");
        assert!(error.to_string().contains("was reaped"), "{error:#}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "storage command timeout should remain bounded"
        );
        let released = released.lock().unwrap().clone();
        assert_eq!(released, ["continue"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn journal_teardown_forgets_only_successful_releases_and_continues() {
        let state = Arc::new(Mutex::new(FakeState {
            active: ["disk-1", "disk-2", "disk-3"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            fail_once: HashSet::from(["disk-2".to_owned()]),
            ..Default::default()
        }));
        let transformer =
            StorageDriverTransformer::new().with_backend(FakeStorage(Arc::clone(&state)));
        let journal = Arc::new(Mutex::new(
            ["disk-1", "disk-2", "disk-3"]
                .into_iter()
                .map(|host| Url::parse(&format!("teststorage://{host}/image")).unwrap())
                .collect::<Vec<_>>(),
        ));
        let mut config = VmConfig::default();
        let journal_to_update = Arc::clone(&journal);
        let mut forget = move |uri: &Url| {
            journal_to_update
                .lock()
                .unwrap()
                .retain(|entry| entry != uri);
            Ok(())
        };

        let first_pass = journal.lock().unwrap().clone();
        let error = transformer
            .teardown_with_cleanup_journal("test-vm", &mut config, &first_pass, &mut forget)
            .expect_err("one release should remain retryable");
        assert!(error.to_string().contains("disk-2"));
        assert_eq!(
            journal
                .lock()
                .unwrap()
                .iter()
                .filter_map(Url::host_str)
                .collect::<Vec<_>>(),
            ["disk-2"]
        );
        assert_eq!(
            state.lock().unwrap().attempts,
            ["disk-1", "disk-2", "disk-3"]
        );

        let second_pass = journal.lock().unwrap().clone();
        transformer
            .teardown_with_cleanup_journal("test-vm", &mut config, &second_pass, &mut forget)
            .expect("remaining entry should be released on retry");
        assert!(journal.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn teardown_attempts_every_disk_aggregates_failures_and_keeps_retry_metadata() {
        let state = Arc::new(Mutex::new(FakeState {
            active: ["disk-1", "disk-2", "disk-3"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            fail_once: ["disk-2", "disk-3"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            ..Default::default()
        }));
        let transformer =
            StorageDriverTransformer::new().with_backend(FakeStorage(Arc::clone(&state)));
        let uris = [
            "teststorage://disk-1/image",
            "teststorage://disk-2/image",
            "teststorage://disk-3/image",
        ];
        let mut config = VmConfig {
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

        let error = transformer
            .teardown("test-vm", &mut config)
            .expect_err("one disk release should fail");
        assert!(error.to_string().contains("teststorage://disk-2/image"));
        assert!(error.to_string().contains("teststorage://disk-3/image"));
        {
            let state = state.lock().unwrap();
            assert_eq!(state.attempts, ["disk-1", "disk-2", "disk-3"]);
            assert_eq!(
                state.active,
                ["disk-2", "disk-3"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            );
        };
        assert_eq!(
            config
                .disks
                .as_ref()
                .unwrap()
                .iter()
                .map(|disk| disk.id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            uris,
            "URI metadata must remain available for retry"
        );

        transformer
            .teardown("test-vm", &mut config)
            .expect("retries should treat prior releases as idempotent");
        let state = state.lock().unwrap();
        assert_eq!(
            state.attempts,
            ["disk-1", "disk-2", "disk-3", "disk-1", "disk-2", "disk-3"]
        );
        assert!(state.active.is_empty());
        drop(state);
    }
}
