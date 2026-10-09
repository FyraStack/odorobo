use crate::ch_driver::transform::ConfigTransform;
use async_trait::async_trait;
use cloud_hypervisor_client::models::VmConfig;
use futures_util::FutureExt;
use stable_eyre::{Report, Result, eyre::eyre};
use std::{
    collections::HashMap,
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::{Mutex, watch};
use tracing::warn;
use url::Url;

mod file;
mod iscsi;
mod rbd;

enum StorageOwnership {
    Owned,
    Borrowed,
    // Appearance after a failed command does not prove who created the resource.
    Uncertain(String),
}

impl StorageOwnership {
    fn uncertainty_error(&self, key: &str) -> Option<Report> {
        let Self::Uncertain(reason) = self else {
            return None;
        };
        Some(eyre!(
            "Storage ownership is uncertain for {key}: {reason}; automatic detach and reuse \
             are blocked. Manual recovery required: stop affected VMs and reconcile the \
             mapping/session with external users; restart the agent to clear quarantine \
             only after all agent storage has been reconciled"
        ))
    }
}

/// Tracks proven ownership separately from uncertain command outcomes. Only
/// proven owned resources may be automatically released; uncertainty quarantines
/// the resource and its VM lease until manual reconciliation.
pub struct StorageAcquisition {
    ownership: StorageOwnership,
    error: Option<Report>,
}

impl StorageAcquisition {
    pub const fn owned() -> Self {
        Self {
            ownership: StorageOwnership::Owned,
            error: None,
        }
    }

    pub const fn borrowed() -> Self {
        Self {
            ownership: StorageOwnership::Borrowed,
            error: None,
        }
    }

    /// Setup failed after ownership was positively established (not just because
    /// a resource appeared after a failed map/login command).
    pub const fn partial(error: Report) -> Self {
        Self {
            ownership: StorageOwnership::Owned,
            error: Some(error),
        }
    }

    pub fn uncertain(error: Report) -> Self {
        Self {
            ownership: StorageOwnership::Uncertain(format!("{error:#}")),
            error: Some(error),
        }
    }

    /// A failed/wait-failed command may have created a resource, or raced an
    /// external creator. Only confirmed absence permits discarding the lease.
    fn after_failed_command(error: Report, exists: Result<bool>) -> Result<Self> {
        match exists {
            Ok(false) => Err(error),
            Ok(true) => Ok(Self::uncertain(error)),
            Err(check_error) => Ok(Self::uncertain(error.wrap_err(format!(
                "Failed to check storage after acquisition command: {check_error:#}"
            )))),
        }
    }
}

/// A release command that was spawned but whose completion is not known. Its
/// daemon/kernel side effect might finish after apparent absence on a retry.
/// This is deliberately distinct from a completed nonzero exit (retryable).
#[derive(Debug, thiserror::Error)]
#[error("Storage release outcome is uncertain: {0:#}")]
#[allow(clippy::redundant_pub_crate)] // Shared explicitly with sibling backends.
pub(crate) struct StorageReleaseUncertain(Report);

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn uncertain_release(error: Report) -> Report {
    StorageReleaseUncertain(error).into()
}

/// A storage backend. Acquisition is separate from path resolution so resources
/// remain owned even if later device discovery fails. Returning an acquisition
/// error means no resource needs tracking. Report proven partial ownership with
/// `StorageAcquisition::partial`, or an ambiguous outcome with `uncertain`.
#[async_trait]
pub trait StorageDriver: Send + Sync {
    fn scheme(&self) -> &'static str;

    /// Canonicalize connection identity before acquisition and retain it for
    /// release. All accepted aliases must use the same key, including the key
    /// reserved before acquisition. If the discovered identity differs, reject
    /// borrowing before side effects or return uncertain after acquisition.
    async fn canonical_uri(&self, uri: &Url) -> Result<Url> {
        let mut uri = uri.clone();
        uri.set_query(None);
        uri.set_fragment(None);
        Ok(uri)
    }

    /// Identity of the resource released by this backend (not necessarily a disk:
    /// iSCSI logout releases an entire session, including all its LUNs).
    fn resource_key(&self, uri: &Url) -> Result<String> {
        Ok(uri.to_string())
    }

    async fn acquire(&self, uri: &Url) -> Result<StorageAcquisition>;

    /// Finds a local path without acquiring any additional resources.
    async fn resolve(&self, uri: &Url) -> Result<PathBuf>;

    /// A completed failure may be retried. A spawned command with unknown
    /// completion must return `uncertain_release(error)` so even apparent
    /// absence on a later retry cannot permit reuse before a late detach.
    async fn release(&self, uri: &Url) -> Result<()>;
}

// Wait deadlines bound the caller, not the backend future. In particular,
// block_in_place/Handle::block_on callers regain control without cancelling a
// map/logout command whose subprocess or daemon may still have side effects.
const OPERATION_WAIT: Duration = Duration::from_secs(30);

struct Operation<T: Clone> {
    result: watch::Sender<Option<std::result::Result<T, String>>>,
}

impl<T: Clone + Send + Sync> Operation<T> {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            result: watch::channel(None).0,
        })
    }

    fn complete(&self, result: Result<T>) {
        // Retain publication even after the last caller has cancelled its wait.
        self.result
            .send_replace(Some(result.map_err(|error| format!("{error:#}"))));
    }

    async fn wait(&self) -> Result<T> {
        let mut result = self.result.subscribe();
        loop {
            let published = result.borrow_and_update().clone();
            if let Some(result) = published {
                return result.map_err(|error| eyre!(error));
            }
            result
                .changed()
                .await
                .map_err(|_| eyre!("Storage operation lost its owner"))?;
        }
    }

    async fn wait_bounded(&self, deadline: Duration) -> Result<T> {
        tokio::time::timeout(deadline, self.wait())
            .await
            .map_err(|_| {
                eyre!(
                    "Storage operation deadline exceeded; the agent still owns the in-flight \
                   operation and retains its VM lease/quarantine. Retry cleanup after completion; \
                   do not reuse or manually detach storage while the operation is in flight"
                )
            })?
    }
}

enum ResourceState {
    Acquiring(Arc<Operation<()>>),
    Ready,
    AcquisitionFailed(String),
    Releasing(Arc<Operation<()>>),
    // A completed failure permits explicit cleanup retry, never a new borrower.
    ReleaseFailed(String),
}

struct Resource {
    uri: Url,
    references: usize,
    ownership: StorageOwnership,
    backend: Arc<dyn StorageDriver>,
    state: ResourceState,
}

#[derive(Default)]
struct StorageRegistry {
    resources: HashMap<String, Resource>,
    leases: HashMap<String, Vec<String>>,
    // Reserve VM ownership before even canonical lookup. Cleanup must not report
    // success while a detached lookup could subsequently acquire a resource.
    resolving: HashMap<String, Vec<Arc<Operation<PathBuf>>>>,
    cleaning: HashMap<String, Arc<Operation<()>>>,
}

impl StorageRegistry {
    fn remove_lease(&mut self, vmid: &str, key: &str) {
        if let Some(leases) = self.leases.get_mut(vmid) {
            if let Some(index) = leases.iter().rposition(|lease| lease == key) {
                leases.remove(index);
            }
            if leases.is_empty() {
                self.leases.remove(vmid);
            }
        }
    }
}

// VM instances have separate transform chains. Registry guards protect only
// short bookkeeping changes; per-key operation states serialize external work.
// Owned tasks survive waiter cancellation and transformer drop. As before, an
// agent process restart requires external reconciliation of all agent storage.
fn shared_registry() -> Arc<Mutex<StorageRegistry>> {
    static REGISTRY: OnceLock<Arc<Mutex<StorageRegistry>>> = OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(Arc::default))
}

/// Resolves disk URIs and records only resources actually acquired by each VM.
/// Config URI fields are descriptive metadata, never evidence of ownership.
pub struct StorageDriverTransformer {
    backends: Vec<Arc<dyn StorageDriver>>,
    registry: Arc<Mutex<StorageRegistry>>,
    operation_wait: Duration,
}

impl StorageDriverTransformer {
    pub fn new() -> Self {
        Self {
            backends: vec![],
            registry: shared_registry(),
            operation_wait: OPERATION_WAIT,
        }
    }

    pub fn with_backend<B: StorageDriver + 'static>(mut self, backend: B) -> Self {
        self.backends.push(Arc::new(backend));
        self
    }

    fn find_backend(&self, uri: &Url) -> Option<&Arc<dyn StorageDriver>> {
        self.backends.iter().find(|b| b.scheme() == uri.scheme())
    }

    async fn resolve_disk(
        &self,
        vmid: &str,
        uri: &Url,
        backend: &Arc<dyn StorageDriver>,
    ) -> Result<PathBuf> {
        let operation = Operation::new();
        let mut registry = self.registry.lock().await;
        if registry.cleaning.contains_key(vmid) || registry.resolving.contains_key(vmid) {
            return Err(eyre!(
                "Storage operation is in flight for VM {vmid}; reuse is blocked"
            ));
        }
        registry
            .resolving
            .entry(vmid.to_owned())
            .or_default()
            .push(Arc::clone(&operation));
        let registry_owner = Arc::clone(&self.registry);
        let vmid = vmid.to_owned();
        let uri = uri.clone();
        let backend = Arc::clone(backend);
        let owned_operation = Arc::clone(&operation);
        // Reserve and spawn without an intervening await: cancellation can never
        // leave a VM reservation with no worker to finish it.
        tokio::spawn(async move {
            let result = AssertUnwindSafe(Self::resolve_owned(
                Arc::clone(&registry_owner),
                &vmid,
                &uri,
                backend,
            ))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(eyre!("Storage resolution worker panicked")));
            let mut registry = registry_owner.lock().await;
            if let Some(pending) = registry.resolving.get_mut(&vmid) {
                pending.retain(|pending| !Arc::ptr_eq(pending, &owned_operation));
                if pending.is_empty() {
                    registry.resolving.remove(&vmid);
                }
            }
            owned_operation.complete(result);
            drop(registry);
        });
        drop(registry);
        operation.wait_bounded(self.operation_wait).await
    }

    async fn resolve_owned(
        registry: Arc<Mutex<StorageRegistry>>,
        vmid: &str,
        uri: &Url,
        backend: Arc<dyn StorageDriver>,
    ) -> Result<PathBuf> {
        let uri = backend.canonical_uri(uri).await?;
        let key = backend.resource_key(&uri)?;
        loop {
            let mut ledger = registry.lock().await;
            if let Some(resource) = ledger.resources.get_mut(&key) {
                if let ResourceState::Acquiring(operation) = &resource.state {
                    let operation = Arc::clone(operation);
                    drop(ledger);
                    operation.wait().await?;
                    continue;
                }
                if let Some(error) = resource.ownership.uncertainty_error(&key) {
                    return Err(error);
                }
                match &resource.state {
                    ResourceState::Acquiring(_) => unreachable!("handled above"),
                    ResourceState::Releasing(_) => {
                        return Err(eyre!(
                            "Storage release is in flight for {key}; reuse is blocked"
                        ));
                    }
                    ResourceState::ReleaseFailed(error)
                    | ResourceState::AcquisitionFailed(error) => {
                        return Err(eyre!(
                            "Storage {key} is quarantined after a failed operation: {error}; retry cleanup before reuse"
                        ));
                    }
                    ResourceState::Ready => {}
                }
                resource.references = resource
                    .references
                    .checked_add(1)
                    .ok_or_else(|| eyre!("Too many storage references for {key}"))?;
                ledger
                    .leases
                    .entry(vmid.to_owned())
                    .or_default()
                    .push(key.clone());
                break;
            }
            let operation = Operation::new();
            ledger.resources.insert(
                key.clone(),
                Resource {
                    uri: uri.clone(),
                    references: 1,
                    ownership: StorageOwnership::Uncertain("acquisition is in flight".to_owned()),
                    backend: Arc::clone(&backend),
                    state: ResourceState::Acquiring(Arc::clone(&operation)),
                },
            );
            // Reserve the actual resource lease before invoking risky acquire.
            ledger
                .leases
                .entry(vmid.to_owned())
                .or_default()
                .push(key.clone());
            // This helper only spawns; no await before the worker owns the lease.
            Self::spawn_acquisition(
                Arc::clone(&registry),
                vmid.to_owned(),
                key.clone(),
                uri.clone(),
                Arc::clone(&backend),
                Arc::clone(&operation),
            );
            drop(ledger);
            operation.wait().await?;
            // The first reference was reserved before acquire, not added again.
            break;
        }
        backend.resolve(&uri).await
    }

    fn spawn_acquisition(
        registry: Arc<Mutex<StorageRegistry>>,
        vmid: String,
        key: String,
        uri: Url,
        backend: Arc<dyn StorageDriver>,
        operation: Arc<Operation<()>>,
    ) {
        tokio::spawn(async move {
            let acquisition = AssertUnwindSafe(backend.acquire(&uri))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Ok(StorageAcquisition::uncertain(eyre!(
                        "Storage acquisition worker panicked"
                    )))
                });
            let mut ledger = registry.lock().await;
            let result = match acquisition {
                Ok(acquisition) => {
                    let error = acquisition
                        .ownership
                        .uncertainty_error(&key)
                        .or(acquisition.error);
                    let resource = ledger
                        .resources
                        .get_mut(&key)
                        .expect("in-flight acquisition retains its resource");
                    resource.ownership = acquisition.ownership;
                    // Uncertain ownership keeps its specific manual-recovery
                    // error. Proven partial ownership can be cleaned, not shared.
                    resource.state = if matches!(resource.ownership, StorageOwnership::Uncertain(_))
                    {
                        ResourceState::Ready
                    } else if let Some(error) = &error {
                        ResourceState::AcquisitionFailed(format!("{error:#}"))
                    } else {
                        ResourceState::Ready
                    };
                    error.map_or(Ok(()), Err)
                }
                Err(error) => {
                    // The backend contract guarantees confirmed absence on
                    // Err; ambiguity must be returned as uncertain instead.
                    ledger.resources.remove(&key);
                    ledger.remove_lease(&vmid, &key);
                    Err(error)
                }
            };
            operation.complete(result);
            drop(ledger);
        });
    }

    /// Release only this VM's leases after confirmed process exit. A cancelled or
    /// timed-out caller leaves the owned cleanup running and blocks VM reuse.
    /// Pre-existing/uncertain mappings are never released; failures retain leases
    /// for explicit retry, including by a reconstructed default transformer.
    pub async fn release_vm(&self, vmid: &str) -> Result<()> {
        let mut registry = self.registry.lock().await;
        let operation = if let Some(operation) = registry.cleaning.get(vmid) {
            Arc::clone(operation)
        } else {
            if !registry.leases.contains_key(vmid) && !registry.resolving.contains_key(vmid) {
                return Ok(());
            }
            let operation = Operation::new();
            registry
                .cleaning
                .insert(vmid.to_owned(), Arc::clone(&operation));
            let pending = registry.resolving.get(vmid).cloned().unwrap_or_default();
            let registry_owner = Arc::clone(&self.registry);
            let vmid = vmid.to_owned();
            let owned_operation = Arc::clone(&operation);
            tokio::spawn(async move {
                let result = AssertUnwindSafe(async {
                    // A lookup/acquire/resolve that outlived transform must finish
                    // before cleanup can clear the VM's quarantine/ownership.
                    for operation in pending {
                        drop(operation.wait().await);
                    }
                    Self::release_owned(Arc::clone(&registry_owner), &vmid).await
                })
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(eyre!("Storage cleanup worker panicked; leases retained")));
                let mut registry = registry_owner.lock().await;
                registry.cleaning.remove(&vmid);
                owned_operation.complete(result);
                drop(registry);
            });
            operation
        };
        drop(registry);
        operation.wait_bounded(self.operation_wait).await
    }

    async fn release_owned(registry: Arc<Mutex<StorageRegistry>>, vmid: &str) -> Result<()> {
        let keys = registry
            .lock()
            .await
            .leases
            .get(vmid)
            .cloned()
            .unwrap_or_default();
        let mut errors = Vec::new();
        for key in keys.into_iter().rev() {
            if let Err(error) = Self::release_lease(Arc::clone(&registry), vmid, &key).await {
                errors.push(format!("{key}: {error:#}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(eyre!("Failed to release storage: {}", errors.join("; ")))
        }
    }

    async fn release_lease(
        registry: Arc<Mutex<StorageRegistry>>,
        vmid: &str,
        key: &str,
    ) -> Result<()> {
        let mut ledger = registry.lock().await;
        let Some(resource) = ledger.resources.get_mut(key) else {
            ledger.remove_lease(vmid, key);
            return Ok(());
        };
        if let ResourceState::Acquiring(operation) | ResourceState::Releasing(operation) =
            &resource.state
        {
            let operation = Arc::clone(operation);
            drop(ledger);
            // The owning task publishes all lease/refcount changes atomically.
            // VM cleanup serialization normally makes this only a release join.
            return operation.wait().await;
        }
        if let Some(error) = resource.ownership.uncertainty_error(key) {
            return Err(error);
        }
        if resource.references > 1 {
            resource.references = resource.references.saturating_sub(1);
            ledger.remove_lease(vmid, key);
            return Ok(());
        }
        if matches!(resource.ownership, StorageOwnership::Borrowed) {
            ledger.resources.remove(key);
            ledger.remove_lease(vmid, key);
            return Ok(());
        }
        let operation = Operation::new();
        resource.state = ResourceState::Releasing(Arc::clone(&operation));
        let backend = Arc::clone(&resource.backend);
        let uri = resource.uri.clone();
        let registry_owner = Arc::clone(&registry);
        let owned_operation = Arc::clone(&operation);
        let key = key.to_owned();
        let vmid = vmid.to_owned();
        tokio::spawn(async move {
            let result = AssertUnwindSafe(backend.release(&uri))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Err(uncertain_release(eyre!("Storage release worker panicked")))
                });
            let mut ledger = registry_owner.lock().await;
            if let Err(error) = &result {
                if let Some(resource) = ledger.resources.get_mut(&key) {
                    if error.downcast_ref::<StorageReleaseUncertain>().is_some() {
                        resource.ownership = StorageOwnership::Uncertain(format!("{error:#}"));
                    }
                    resource.state = ResourceState::ReleaseFailed(format!("{error:#}"));
                }
            } else {
                ledger.resources.remove(&key);
                ledger.remove_lease(&vmid, &key);
            }
            owned_operation.complete(result);
            drop(ledger);
        });
        drop(ledger);
        operation.wait().await
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

fn parse_storage_uri(path: &str) -> Result<Option<Url>> {
    if path.starts_with("rbd://") {
        // Preserve the raw syntax check so URL normalization cannot turn a
        // nested/dot-segment path into a different, apparently valid image.
        rbd::RbdImage::parse_uri(path)?;
    }
    match Url::parse(path) {
        Ok(uri) => Ok(Some(uri)),
        Err(error)
            if ["file://", "rbd://", "iscsi://"]
                .iter()
                .any(|prefix| path.starts_with(prefix)) =>
        {
            Err(eyre!("Invalid supported storage URI: {error}"))
        }
        Err(_) => Ok(None),
    }
}

impl ConfigTransform for StorageDriverTransformer {
    fn teardown(&self, vmid: &str, _config: &mut VmConfig) -> Result<()> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.release_vm(vmid))
        })
    }

    fn transform(&self, vmid: &str, config: &mut VmConfig) -> Result<()> {
        let Some(disks) = config.disks.as_mut() else {
            return Ok(());
        };

        for disk in disks {
            let Some(path) = disk.path.as_deref() else {
                continue;
            };
            let Some(uri) = parse_storage_uri(path)? else {
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
            let resolved = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(self.resolve_disk(vmid, &uri, backend))
            })?;
            let new_disk_id = uri
                .clone()
                .query_pairs_mut()
                .append_pair("id", disk.id.as_deref().unwrap_or("<unknown>"))
                .finish()
                .to_string();
            disk.id = Some(new_disk_id);
            disk.path = Some(resolved.to_string_lossy().into_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_hypervisor_client::models::DiskConfig;
    use std::sync::{Arc, Mutex as StdMutex};

    #[derive(Default)]
    struct MockState {
        acquisitions: Vec<String>,
        releases: Vec<String>,
        fail_release_once: bool,
        registry: Arc<Mutex<StorageRegistry>>,
    }

    struct MockStorage(Arc<StdMutex<MockState>>);

    #[async_trait]
    impl StorageDriver for MockStorage {
        fn scheme(&self) -> &'static str {
            "mock"
        }

        async fn acquire(&self, uri: &Url) -> Result<StorageAcquisition> {
            self.0.lock().unwrap().acquisitions.push(uri.to_string());
            match uri.path() {
                "/fail-acquire" => Err(eyre!("acquisition failed without resources")),
                "/partial" => Ok(StorageAcquisition::partial(eyre!("partial acquisition"))),
                // Deterministic failed-command races: absence was checked before
                // launching, but a resource appeared or discovery failed after.
                "/uncertain" => StorageAcquisition::after_failed_command(
                    eyre!("map/login failed while an external resource appeared"),
                    Ok(true),
                ),
                "/uncertain-check" => StorageAcquisition::after_failed_command(
                    eyre!("map/login failed"),
                    Err(eyre!("follow-up discovery failed")),
                ),
                "/uncertain-wait" => {
                    StorageAcquisition::after_failed_command(eyre!("command wait failed"), Ok(true))
                }
                "/failed-command-absent" => StorageAcquisition::after_failed_command(
                    eyre!("map/login failed, resource absent"),
                    Ok(false),
                ),
                "/borrowed" => Ok(StorageAcquisition::borrowed()),
                _ => Ok(StorageAcquisition::owned()),
            }
        }

        async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
            if uri.path() == "/fail-resolve" {
                return Err(eyre!("device discovery failed"));
            }
            Ok(PathBuf::from("/dev/mock"))
        }

        async fn release(&self, uri: &Url) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.releases.push(uri.to_string());
            if std::mem::take(&mut state.fail_release_once) {
                return Err(eyre!("release failed"));
            }
            drop(state);
            Ok(())
        }
    }

    fn transformer(state: &Arc<StdMutex<MockState>>) -> StorageDriverTransformer {
        let mut transformer = StorageDriverTransformer::new()
            .with_backend(file::FileStorage)
            .with_backend(MockStorage(Arc::clone(state)));
        transformer.registry = Arc::clone(&state.lock().unwrap().registry);
        transformer
    }

    struct Gate {
        entered: tokio::sync::Semaphore,
        proceed: tokio::sync::Semaphore,
    }

    impl Default for Gate {
        fn default() -> Self {
            Self {
                entered: tokio::sync::Semaphore::new(0),
                proceed: tokio::sync::Semaphore::new(0),
            }
        }
    }

    impl Gate {
        async fn block(&self) {
            self.entered.add_permits(1);
            self.proceed.acquire().await.unwrap().forget();
        }

        async fn entered(&self) {
            tokio::time::timeout(Duration::from_secs(2), self.entered.acquire())
                .await
                .expect("worker did not reach controlled side effect")
                .unwrap()
                .forget();
        }

        fn finish(&self) {
            self.proceed.add_permits(1);
        }
    }

    #[derive(Default)]
    struct ControlledState {
        canonical: Option<Arc<Gate>>,
        acquire: Option<Arc<Gate>>,
        resolve: Option<Arc<Gate>>,
        release: Option<Arc<Gate>>,
        acquisitions: StdMutex<Vec<String>>,
        releases: StdMutex<Vec<String>>,
        uncertain_acquire: bool,
        uncertain_release: bool,
        fail_release_once: std::sync::atomic::AtomicBool,
    }

    struct ControlledStorage(Arc<ControlledState>);

    #[async_trait]
    impl StorageDriver for ControlledStorage {
        fn scheme(&self) -> &'static str {
            "controlled"
        }

        async fn canonical_uri(&self, uri: &Url) -> Result<Url> {
            if uri.path() == "/a"
                && let Some(gate) = &self.0.canonical
            {
                gate.block().await;
            }
            Ok(uri.clone())
        }

        async fn acquire(&self, uri: &Url) -> Result<StorageAcquisition> {
            if uri.path() == "/a"
                && let Some(gate) = &self.0.acquire
            {
                gate.block().await;
            }
            // Record the actual side effect AFTER the caller may have cancelled.
            self.0.acquisitions.lock().unwrap().push(uri.to_string());
            if self.0.uncertain_acquire {
                Ok(StorageAcquisition::uncertain(eyre!(
                    "late map outcome unknown"
                )))
            } else {
                Ok(StorageAcquisition::owned())
            }
        }

        async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
            if uri.path() == "/a"
                && let Some(gate) = &self.0.resolve
            {
                gate.block().await;
            }
            Ok(PathBuf::from("/dev/controlled"))
        }

        async fn release(&self, uri: &Url) -> Result<()> {
            if uri.path() == "/a"
                && let Some(gate) = &self.0.release
            {
                gate.block().await;
            }
            self.0.releases.lock().unwrap().push(uri.to_string());
            if self.0.uncertain_release {
                Err(uncertain_release(eyre!("late unmap outcome unknown")))
            } else if self
                .0
                .fail_release_once
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                Err(eyre!("completed release failed"))
            } else {
                Ok(())
            }
        }
    }

    fn controlled(state: Arc<ControlledState>, wait: Duration) -> Arc<StorageDriverTransformer> {
        let mut transform = StorageDriverTransformer::new().with_backend(ControlledStorage(state));
        transform.registry = Arc::default();
        transform.operation_wait = wait;
        Arc::new(transform)
    }

    async fn controlled_resolve(
        transform: &StorageDriverTransformer,
        vmid: &str,
        path: &str,
    ) -> Result<PathBuf> {
        let uri = Url::parse(&format!("controlled://storage/{path}")).unwrap();
        transform
            .resolve_disk(vmid, &uri, transform.find_backend(&uri).unwrap())
            .await
    }

    #[test]
    fn malformed_supported_storage_uri_syntax_is_not_passed_through() {
        for path in ["rbd://[", "file://[", "rbd://pool/../other-image"] {
            assert!(parse_storage_uri(path).is_err(), "accepted {path}");
        }
        assert!(
            parse_storage_uri("/var/lib/odorobo/disk.raw")
                .unwrap()
                .is_none()
        );
        assert!(
            parse_storage_uri("custom://storage/disk")
                .unwrap()
                .is_some()
        );
        assert!(
            parse_storage_uri("rbd://UPPER/image")
                .unwrap_err()
                .to_string()
                .contains("must be lowercase")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_rbd_uri_is_rejected_before_cloud_hypervisor() {
        let transformer = StorageDriverTransformer::default();
        let mut config = config(&["rbd://"]);
        let error = transformer
            .transform("malformed-rbd", &mut config)
            .expect_err("an invalid RBD source must not reach Cloud Hypervisor");
        assert!(error.to_string().contains("RBD URI"));
    }

    fn config(paths: &[&str]) -> VmConfig {
        VmConfig {
            disks: Some(
                paths
                    .iter()
                    .map(|path| DiskConfig {
                        path: Some((*path).to_owned()),
                        id: Some("disk".to_owned()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_acquire_keeps_lease_and_does_not_block_unrelated_key() {
        let gate = Arc::new(Gate::default());
        let state = Arc::new(ControlledState {
            acquire: Some(Arc::clone(&gate)),
            ..Default::default()
        });
        let transform = controlled(Arc::clone(&state), Duration::from_secs(2));
        let caller = tokio::spawn({
            let transform = Arc::clone(&transform);
            async move { controlled_resolve(&transform, "owner", "a").await }
        });
        gate.entered().await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let ledger = transform.registry.lock().await;
        assert_eq!(ledger.resources["controlled://storage/a"].references, 1);
        assert_eq!(ledger.leases["owner"], ["controlled://storage/a"]);
        assert!(ledger.resolving.contains_key("owner"));
        drop(ledger);
        let error = controlled_resolve(&transform, "owner", "b")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("reuse is blocked"));
        tokio::time::timeout(
            Duration::from_secs(1),
            controlled_resolve(&transform, "unrelated", "b"),
        )
        .await
        .expect("pending A must not block B")
        .unwrap();
        transform.release_vm("unrelated").await.unwrap();
        let waiter = tokio::spawn({
            let transform = Arc::clone(&transform);
            async move { controlled_resolve(&transform, "borrower", "a").await }
        });
        let waiter_operation = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(operation) = transform.registry.lock().await.resolving.get("borrower") {
                    break Arc::clone(&operation[0]);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("borrower worker did not register its reservation");
        assert!(waiter_operation.result.borrow().is_none());
        gate.finish();
        waiter.await.unwrap().unwrap();
        assert_eq!(
            state.acquisitions.lock().unwrap().as_slice(),
            ["controlled://storage/b", "controlled://storage/a"]
        );
        assert_eq!(
            transform.registry.lock().await.resources["controlled://storage/a"].references,
            2
        );
        transform.release_vm("owner").await.unwrap();
        assert_eq!(state.releases.lock().unwrap().len(), 1); // B only
        transform.release_vm("borrower").await.unwrap();
        transform.release_vm("borrower").await.unwrap();
        assert_eq!(
            state.releases.lock().unwrap().as_slice(),
            ["controlled://storage/b", "controlled://storage/a"]
        );
        assert!(transform.registry.lock().await.resources.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_release_cannot_late_detach_a_new_borrower() {
        let gate = Arc::new(Gate::default());
        let state = Arc::new(ControlledState {
            release: Some(Arc::clone(&gate)),
            ..Default::default()
        });
        let transform = controlled(Arc::clone(&state), Duration::from_millis(30));
        controlled_resolve(&transform, "owner", "a").await.unwrap();
        let caller = tokio::spawn({
            let transform = Arc::clone(&transform);
            async move { transform.release_vm("owner").await }
        });
        gate.entered().await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let error = controlled_resolve(&transform, "new-vm", "a")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("release is in flight"));
        assert!(
            !transform
                .registry
                .lock()
                .await
                .leases
                .contains_key("new-vm")
        );
        let retry_error = transform.release_vm("owner").await.unwrap_err();
        assert!(retry_error.to_string().contains("deadline exceeded"));
        let cleanup = Arc::clone(&transform.registry.lock().await.cleaning["owner"]);
        assert_eq!(
            transform.registry.lock().await.resources["controlled://storage/a"].references,
            1
        );
        tokio::time::timeout(
            Duration::from_secs(1),
            controlled_resolve(&transform, "unrelated", "b"),
        )
        .await
        .expect("pending release A must not block B")
        .unwrap();
        transform.release_vm("unrelated").await.unwrap();
        assert_eq!(state.acquisitions.lock().unwrap().len(), 2);
        assert_eq!(
            state.releases.lock().unwrap().as_slice(),
            ["controlled://storage/b"]
        );
        gate.finish();
        cleanup.wait().await.unwrap();
        transform.release_vm("owner").await.unwrap();
        assert_eq!(
            state.releases.lock().unwrap().as_slice(),
            ["controlled://storage/b", "controlled://storage/a"]
        );
        controlled_resolve(&transform, "new-vm", "a").await.unwrap();
        assert_eq!(state.acquisitions.lock().unwrap().len(), 3); // fresh acquisition, AFTER release
        gate.finish();
        transform.release_vm("new-vm").await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn synchronous_transform_deadline_covers_lookup_acquire_and_resolve() {
        for stage in ["lookup", "acquire", "resolve"] {
            let gate = Arc::new(Gate::default());
            let state = Arc::new(ControlledState {
                canonical: (stage == "lookup").then(|| Arc::clone(&gate)),
                acquire: (stage == "acquire").then(|| Arc::clone(&gate)),
                resolve: (stage == "resolve").then(|| Arc::clone(&gate)),
                ..Default::default()
            });
            let transform = controlled(Arc::clone(&state), Duration::from_millis(30));
            let mut vm_config = config(&["controlled://storage/a"]);
            let started = std::time::Instant::now();
            let error = transform.transform("owner", &mut vm_config).unwrap_err();
            assert!(
                error.to_string().contains("deadline exceeded"),
                "{stage}: {error:#}"
            );
            assert!(started.elapsed() < Duration::from_secs(1));
            gate.entered().await;
            let ledger = transform.registry.lock().await;
            assert!(ledger.resolving.contains_key("owner"));
            if stage == "lookup" {
                assert!(ledger.resources.is_empty());
            } else {
                assert_eq!(ledger.resources["controlled://storage/a"].references, 1);
                assert_eq!(ledger.leases["owner"], ["controlled://storage/a"]);
            }
            drop(ledger);
            let error = transform.teardown("owner", &mut vm_config).unwrap_err();
            assert!(error.to_string().contains("deadline exceeded"));
            let cleanup = Arc::clone(&transform.registry.lock().await.cleaning["owner"]);
            let error = controlled_resolve(&transform, "owner", "b")
                .await
                .unwrap_err();
            assert!(error.to_string().contains("reuse is blocked"));
            controlled_resolve(&transform, "unrelated", "b")
                .await
                .unwrap();
            transform.release_vm("unrelated").await.unwrap();
            gate.finish();
            cleanup.wait().await.unwrap();
            transform.teardown("owner", &mut vm_config).unwrap();
            let ledger = transform.registry.lock().await;
            assert!(ledger.resources.is_empty());
            assert!(ledger.leases.is_empty());
            assert!(ledger.resolving.is_empty());
            assert!(ledger.cleaning.is_empty());
            drop(ledger);
            assert_eq!(state.acquisitions.lock().unwrap().len(), 2);
            assert_eq!(state.releases.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timed_out_acquire_that_finishes_uncertain_never_detaches_or_shares() {
        let gate = Arc::new(Gate::default());
        let state = Arc::new(ControlledState {
            acquire: Some(Arc::clone(&gate)),
            uncertain_acquire: true,
            ..Default::default()
        });
        let transform = controlled(Arc::clone(&state), Duration::from_millis(30));
        let error = controlled_resolve(&transform, "owner", "a")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("deadline exceeded"));
        gate.entered().await;
        let resolve = Arc::clone(&transform.registry.lock().await.resolving["owner"][0]);
        gate.finish();
        let error = resolve.wait().await.unwrap_err();
        assert!(error.to_string().contains("Manual recovery required"));
        for _ in 0..2 {
            let error = transform.release_vm("owner").await.unwrap_err();
            assert!(error.to_string().contains("late map outcome unknown"));
            assert!(error.to_string().contains("Manual recovery required"));
        }
        let error = controlled_resolve(&transform, "new-vm", "a")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Manual recovery required"));
        let ledger = transform.registry.lock().await;
        assert_eq!(ledger.resources["controlled://storage/a"].references, 1);
        assert_eq!(ledger.leases["owner"], ["controlled://storage/a"]);
        assert!(!ledger.leases.contains_key("new-vm"));
        drop(ledger);
        assert_eq!(state.acquisitions.lock().unwrap().len(), 1);
        assert!(state.releases.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timed_out_release_uncertainty_survives_retry_and_transformer_drop() {
        let gate = Arc::new(Gate::default());
        let state = Arc::new(ControlledState {
            release: Some(Arc::clone(&gate)),
            uncertain_release: true,
            ..Default::default()
        });
        let transform = controlled(Arc::clone(&state), Duration::from_millis(30));
        controlled_resolve(&transform, "owner", "a").await.unwrap();
        let error = transform.release_vm("owner").await.unwrap_err();
        assert!(error.to_string().contains("deadline exceeded"));
        gate.entered().await;
        let cleanup = Arc::clone(&transform.registry.lock().await.cleaning["owner"]);
        let registry = Arc::clone(&transform.registry);
        drop(transform);
        gate.finish();
        let error = cleanup.wait().await.unwrap_err();
        assert!(error.to_string().contains("late unmap outcome unknown"));
        let retry = StorageDriverTransformer {
            registry,
            operation_wait: Duration::from_secs(1),
            ..StorageDriverTransformer::default()
        };
        for _ in 0..2 {
            let error = retry.release_vm("owner").await.unwrap_err();
            assert!(error.to_string().contains("Manual recovery required"));
        }
        assert_eq!(state.releases.lock().unwrap().len(), 1);
        let replacement = StorageDriverTransformer {
            registry: Arc::clone(&retry.registry),
            ..StorageDriverTransformer::new().with_backend(ControlledStorage(Arc::clone(&state)))
        };
        let error = controlled_resolve(&replacement, "new-vm", "a")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Manual recovery required"));
        assert_eq!(state.acquisitions.lock().unwrap().len(), 1);
        let ledger = retry.registry.lock().await;
        assert_eq!(ledger.resources["controlled://storage/a"].references, 1);
        assert_eq!(ledger.leases["owner"], ["controlled://storage/a"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn completed_failed_release_blocks_borrowers_until_explicit_retry() {
        let state = Arc::new(ControlledState::default());
        let transform = controlled(Arc::clone(&state), Duration::from_secs(2));
        controlled_resolve(&transform, "owner", "a").await.unwrap();
        state
            .fail_release_once
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = transform.release_vm("owner").await.unwrap_err();
        assert!(error.to_string().contains("completed release failed"));
        let error = controlled_resolve(&transform, "new-vm", "a")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("retry cleanup before reuse"));
        assert_eq!(
            transform.registry.lock().await.resources["controlled://storage/a"].references,
            1
        );
        transform.release_vm("owner").await.unwrap();
        transform.release_vm("owner").await.unwrap();
        assert!(transform.registry.lock().await.leases.is_empty());
        controlled_resolve(&transform, "new-vm", "a").await.unwrap();
        assert_eq!(state.acquisitions.lock().unwrap().len(), 2);
        transform.release_vm("new-vm").await.unwrap();
        assert_eq!(state.releases.lock().unwrap().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn first_failure_does_not_release_untouched_later_disk() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        let transform = transformer(&state);
        let mut config = config(&["file://remote-host/invalid", "mock://untouched/borrowed"]);
        let error = transform.transform("failed", &mut config).unwrap_err();
        assert!(error.to_string().contains("Failed to convert file URI"));
        transform.teardown("failed", &mut config).unwrap();
        let state = state.lock().unwrap();
        assert!(state.acquisitions.is_empty());
        assert!(state.releases.is_empty());
        drop(state);
        assert_eq!(config.disks.unwrap()[0].id.as_deref(), Some("disk"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn second_failure_releases_only_successful_first_disk() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        let transform = transformer(&state);
        let mut config = config(&[
            "mock://second-failure/first",
            "mock://second-failure/fail-acquire",
            "mock://second-failure/untouched",
        ]);
        let error = transform.transform("failed", &mut config).unwrap_err();
        assert!(error.to_string().contains("acquisition failed"));
        transform.teardown("failed", &mut config).unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.acquisitions.len(), 2);
        assert_eq!(state.releases, ["mock://second-failure/first"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn borrowed_external_mapping_is_never_released() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        let transform = transformer(&state);
        let mut config = config(&["mock://external/borrowed", "mock://external/fail-acquire"]);
        let error = transform.transform("borrower", &mut config).unwrap_err();
        assert!(error.to_string().contains("acquisition failed"));
        transform.teardown("borrower", &mut config).unwrap();
        assert!(state.lock().unwrap().releases.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shared_owned_mapping_is_released_only_by_last_vm() {
        for owner_first in [false, true] {
            let state = Arc::new(StdMutex::new(MockState::default()));
            let owner = transformer(&state);
            let borrower = transformer(&state);
            let mut owner_config = config(&["mock://shared/disk"]);
            let mut borrower_config = config(&["mock://shared/disk?id=other"]);
            owner.transform("owner", &mut owner_config).unwrap();
            borrower
                .transform("borrower", &mut borrower_config)
                .unwrap();
            assert_eq!(state.lock().unwrap().acquisitions.len(), 1);
            let (first, first_id, first_config, last, last_id, last_config) = if owner_first {
                (
                    &owner,
                    "owner",
                    &mut owner_config,
                    &borrower,
                    "borrower",
                    &mut borrower_config,
                )
            } else {
                (
                    &borrower,
                    "borrower",
                    &mut borrower_config,
                    &owner,
                    "owner",
                    &mut owner_config,
                )
            };
            first.teardown(first_id, first_config).unwrap();
            assert!(state.lock().unwrap().releases.is_empty());
            last.teardown(last_id, last_config).unwrap();
            assert_eq!(state.lock().unwrap().releases, ["mock://shared/disk"]);
            last.teardown(last_id, last_config).unwrap();
            assert_eq!(state.lock().unwrap().releases.len(), 1);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_disk_leases_are_released_exactly_once() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        let transform = transformer(&state);
        let mut config = config(&["mock://duplicate/disk", "mock://duplicate/disk?id=second"]);
        transform.transform("vm", &mut config).unwrap();
        assert_eq!(state.lock().unwrap().acquisitions.len(), 1);
        transform.teardown("vm", &mut config).unwrap();
        transform.teardown("vm", &mut config).unwrap();
        assert_eq!(state.lock().unwrap().releases, ["mock://duplicate/disk"]);
    }

    #[tokio::test]
    async fn concurrent_vms_share_one_acquisition_and_release() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        let first = transformer(&state);
        let second = transformer(&state);
        let uri = Url::parse("mock://concurrent/disk").unwrap();
        let (first_result, second_result) = tokio::join!(
            first.resolve_disk("first", &uri, first.find_backend(&uri).unwrap()),
            second.resolve_disk("second", &uri, second.find_backend(&uri).unwrap()),
        );
        assert_eq!(first_result.unwrap(), PathBuf::from("/dev/mock"));
        assert_eq!(second_result.unwrap(), PathBuf::from("/dev/mock"));
        assert_eq!(state.lock().unwrap().acquisitions.len(), 1);
        let (first_result, second_result) =
            tokio::join!(first.release_vm("first"), second.release_vm("second"));
        first_result.unwrap();
        second_result.unwrap();
        assert_eq!(state.lock().unwrap().releases, ["mock://concurrent/disk"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_borrower_cannot_release_running_owner_mapping() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        let owner = transformer(&state);
        let borrower = transformer(&state);
        let mut owner_config = config(&["mock://failed-borrower/shared"]);
        let mut borrower_config = config(&[
            "mock://failed-borrower/shared",
            "mock://failed-borrower/fail-acquire",
        ]);
        owner.transform("running", &mut owner_config).unwrap();
        let error = borrower
            .transform("failed", &mut borrower_config)
            .unwrap_err();
        assert!(error.to_string().contains("acquisition failed"));
        borrower.teardown("failed", &mut borrower_config).unwrap();
        assert!(state.lock().unwrap().releases.is_empty());
        owner.teardown("running", &mut owner_config).unwrap();
        assert_eq!(
            state.lock().unwrap().releases,
            ["mock://failed-borrower/shared"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn partial_acquisitions_and_path_resolution_failures_are_released() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        for path in ["mock://partial/partial", "mock://partial/fail-resolve"] {
            let transform = transformer(&state);
            let mut config = config(&[path]);
            let error = transform.transform("failed", &mut config).unwrap_err();
            assert!(
                error.to_string().contains("partial acquisition")
                    || error.to_string().contains("device discovery failed")
            );
            // Teardown is independent of the partially transformed config.
            transform
                .teardown("failed", &mut VmConfig::default())
                .unwrap();
        }
        assert_eq!(
            state.lock().unwrap().releases,
            ["mock://partial/partial", "mock://partial/fail-resolve"]
        );
    }

    #[test]
    fn failed_command_only_confirmed_absence_discards_acquisition() {
        for command_error in [
            "map failed",
            "login failed (exit 15)",
            "command wait failed",
        ] {
            let error = StorageAcquisition::after_failed_command(eyre!(command_error), Ok(false))
                .err()
                .expect("confirmed absence needs no lease");
            assert_eq!(error.to_string(), command_error);
            for exists in [Ok(true), Err(eyre!("follow-up check failed"))] {
                let acquisition =
                    StorageAcquisition::after_failed_command(eyre!(command_error), exists).unwrap();
                let StorageOwnership::Uncertain(reason) = acquisition.ownership else {
                    panic!("failure without confirmed absence must be uncertain");
                };
                assert!(reason.contains(command_error));
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uncertain_acquisition_is_quarantined_never_released_or_shared() {
        for (path, reason) in [
            ("uncertain", "external resource appeared"),
            ("uncertain-check", "follow-up discovery failed"),
            ("uncertain-wait", "command wait failed"),
        ] {
            let state = Arc::new(StdMutex::new(MockState::default()));
            let owner = transformer(&state);
            let uri = format!("mock://quarantine/{path}");
            let mut owner_config = config(&["mock://quarantine/owned", &uri]);
            let error = owner.transform("failed", &mut owner_config).unwrap_err();
            assert!(error.to_string().contains(reason));
            assert!(error.to_string().contains("Manual recovery required"));
            let error = owner.teardown("failed", &mut owner_config).unwrap_err();
            assert!(error.to_string().contains(reason));
            // Proven acquisitions still clean up; uncertain ones never detach.
            assert_eq!(state.lock().unwrap().releases, ["mock://quarantine/owned"]);
            drop(owner);

            let next = transformer(&state);
            for vmid in ["failed", "another-vm"] {
                let mut next_config = config(&[&format!("{uri}?id=other")]);
                let error = next.transform(vmid, &mut next_config).unwrap_err();
                assert!(error.to_string().contains(reason));
                assert!(
                    error
                        .to_string()
                        .contains("automatic detach and reuse are blocked")
                );
            }
            // A rejected VM has no lease to release and cannot clear quarantine.
            next.teardown("another-vm", &mut VmConfig::default())
                .unwrap();
            let error = next
                .teardown("failed", &mut VmConfig::default())
                .unwrap_err();
            assert!(error.to_string().contains("Manual recovery required"));
            let registry = next.registry.lock().await;
            assert_eq!(registry.resources[&uri].references, 1);
            assert_eq!(
                registry.leases["failed"].as_slice(),
                std::slice::from_ref(&uri)
            );
            assert!(!registry.leases.contains_key("another-vm"));
            let StorageOwnership::Uncertain(retained) = &registry.resources[&uri].ownership else {
                panic!("cleanup must retain uncertainty");
            };
            assert!(retained.contains(reason));
            drop(registry);
            assert_eq!(state.lock().unwrap().acquisitions.len(), 2);
            assert_eq!(state.lock().unwrap().releases, ["mock://quarantine/owned"]);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn confirmed_absence_after_failed_command_does_not_quarantine() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        let transform = transformer(&state);
        let mut config = config(&["mock://absent/failed-command-absent"]);
        let error = transform.transform("failed", &mut config).unwrap_err();
        assert!(error.to_string().contains("resource absent"));
        transform.teardown("failed", &mut config).unwrap();
        assert!(transform.registry.lock().await.resources.is_empty());
        assert!(transform.registry.lock().await.leases.is_empty());
        assert!(state.lock().unwrap().releases.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn default_transformer_retains_global_uncertainty_after_original_drop() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        let transform =
            StorageDriverTransformer::new().with_backend(MockStorage(Arc::clone(&state)));
        let vmid = ulid::Ulid::generate().to_string();
        let uri = format!("mock://{vmid}/uncertain-check");
        let mut config = config(&[&uri]);
        let error = transform.transform(&vmid, &mut config).unwrap_err();
        assert!(error.to_string().contains("follow-up discovery failed"));
        drop(transform);
        for _ in 0..2 {
            let error = StorageDriverTransformer::default()
                .teardown(&vmid, &mut VmConfig::default())
                .unwrap_err();
            assert!(error.to_string().contains("follow-up discovery failed"));
            assert!(error.to_string().contains("Manual recovery required"));
        }
        let next = StorageDriverTransformer::new().with_backend(MockStorage(Arc::clone(&state)));
        let error = next.transform("other-vm", &mut config).unwrap_err();
        assert!(error.to_string().contains("follow-up discovery failed"));
        assert_eq!(state.lock().unwrap().acquisitions.len(), 1);
        assert!(state.lock().unwrap().releases.is_empty());
        let mut registry = next.registry.lock().await;
        assert_eq!(registry.resources[&uri].references, 1);
        assert_eq!(
            registry.leases[&vmid].as_slice(),
            std::slice::from_ref(&uri)
        );
        // Test-only reset of this unique key; production never clears uncertainty.
        registry.resources.remove(&uri);
        registry.leases.remove(&vmid);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn default_transformer_retries_global_failed_release_after_original_drop() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        let transform =
            StorageDriverTransformer::new().with_backend(MockStorage(Arc::clone(&state)));
        let vmid = ulid::Ulid::generate().to_string();
        let uri = format!("mock://global-retry/{vmid}");
        let mut config = config(&[&uri]);
        transform.transform(&vmid, &mut config).unwrap();
        state.lock().unwrap().fail_release_once = true;
        let error = transform.teardown(&vmid, &mut config).unwrap_err();
        assert!(error.to_string().contains("release failed"));
        drop(transform);
        StorageDriverTransformer::default()
            .teardown(&vmid, &mut VmConfig::default())
            .unwrap();
        StorageDriverTransformer::default()
            .teardown(&vmid, &mut VmConfig::default())
            .unwrap();
        assert_eq!(state.lock().unwrap().releases, [uri.clone(), uri]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn teardown_reports_failure_continues_and_retains_failed_lease_for_retry() {
        let state = Arc::new(StdMutex::new(MockState::default()));
        let transform = transformer(&state);
        let mut config = config(&["mock://retry/first", "mock://retry/second"]);
        transform.transform("vm", &mut config).unwrap();
        state.lock().unwrap().fail_release_once = true;
        let error = transform.teardown("vm", &mut config).unwrap_err();
        assert!(error.to_string().contains("release failed"));
        assert_eq!(
            state.lock().unwrap().releases,
            ["mock://retry/second", "mock://retry/first"]
        );
        drop(transform);
        let retry = StorageDriverTransformer {
            registry: Arc::clone(&state.lock().unwrap().registry),
            ..StorageDriverTransformer::default()
        };
        // Even the default transformer, with no mock backend registered, can
        // release the retained backend after the original transform is dropped.
        retry.teardown("vm", &mut VmConfig::default()).unwrap();
        assert_eq!(
            state.lock().unwrap().releases,
            [
                "mock://retry/second",
                "mock://retry/first",
                "mock://retry/second"
            ]
        );
        retry.teardown("vm", &mut config).unwrap();
        assert_eq!(state.lock().unwrap().releases.len(), 3);
    }
}
