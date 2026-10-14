//! Durable desired cluster state.
//!
//! Keys are versioned and namespaced as `/odorobo/v1/{kind}/{id}`. Values are
//! JSON records with a `version` field so readers can reject unknown versions
//! rather than silently misinterpreting state.

use std::{collections::BTreeMap, fmt::Display, sync::Arc, time::Duration};

use async_trait::async_trait;
use etcd_client::{
    Certificate, Client, Compare, CompareOp, ConnectOptions, GetOptions, TlsOptions, Txn, TxnOp,
    TxnOpResponse,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::sync::RwLock;
use ulid::Ulid;

pub const KEY_PREFIX: &str = "/odorobo/v1";
pub const VM_MANIFESTS_PREFIX: &str = "/odorobo/v1/vm-manifests";
pub const PLACEMENT_PREFIX: &str = "/odorobo/v1/placement";
pub const NODE_STATE_PREFIX: &str = "/odorobo/v1/node-state";
pub const OPERATIONS_PREFIX: &str = "/odorobo/v1/operations";
pub const RECORD_VERSION: u16 = 1;

/// Lifecycle of an explicitly managed VM placement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementLifecycle {
    #[default]
    Active,
    Stopping,
    Deleting,
}

/// The agent hostname selected to run a VM manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRecord {
    pub vmid: Ulid,
    pub node: String,
    /// Fences delayed requests from a later incarnation; absent on legacy records.
    #[serde(default)]
    pub generation: Option<Ulid>,
    /// Older version-1 records are active unless explicitly marked otherwise.
    #[serde(default)]
    pub lifecycle: PlacementLifecycle,
}

/// Version-1 payload for `/odorobo/v1/node-state/<node>` records.
/// The enclosing store record supplies the version wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStateRecord {
    /// Stable node identifier, matching the key suffix.
    pub node: String,
    /// User-visible labels and annotations associated with the node.
    #[serde(default)]
    pub metadata: crate::types::ObjectMetadata,
}

/// Status of a version-1 operation record; this is descriptive state only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Pending,
    Running,
    Succeeded,
    Failed,
}

/// Version-1 payload for `/odorobo/v1/operations/<operation_id>` records.
/// Operation processing and state transitions are intentionally unspecified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRecord {
    /// Stable operation identifier, matching the key suffix.
    pub operation_id: Ulid,
    /// Opaque operation kind understood by a future operation processor.
    pub kind: String,
    /// Opaque target identifier (for example, a VM ID).
    pub target: String,
    pub state: OperationState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmState<T> {
    pub manifest: T,
    pub placement: PlacementRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopIntent {
    Shutdown,
    Delete,
}

impl StopIntent {
    const fn lifecycle(self) -> PlacementLifecycle {
        match self {
            Self::Shutdown => PlacementLifecycle::Stopping,
            Self::Delete => PlacementLifecycle::Deleting,
        }
    }
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("state store operation failed: {0}")]
    Backend(String),
    #[error("state record has unsupported version {0}")]
    UnsupportedVersion(u16),
    #[error("state record is missing")]
    Missing,
    #[error("state record already exists with different intent")]
    AlreadyExists,
    #[error("manifest and placement are incomplete or inconsistent")]
    Incomplete,
    #[error("state changed concurrently or stop lifecycle conflicts")]
    Conflict,
    #[error("state serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// A VM record pair that could not be decoded. It is reported, not loaded,
/// so one damaged VM does not make every other VM unmanageable.
#[derive(Debug)]
pub struct InvalidVmState {
    /// Key suffix identifying the VM (normally its ULID).
    pub id: String,
    pub error: StateError,
}

/// Paired VM state read from one store snapshot.
#[derive(Debug)]
pub struct VmStateSnapshot<T> {
    pub states: Vec<VmState<T>>,
    pub invalid: Vec<InvalidVmState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VersionedRecord<T> {
    version: u16,
    value: T,
}

fn decode_record<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StateError> {
    let record: VersionedRecord<serde_json::Value> = serde_json::from_slice(bytes)?;
    if record.version != RECORD_VERSION {
        return Err(StateError::UnsupportedVersion(record.version));
    }
    Ok(serde_json::from_value(record.value)?)
}

fn encode_record<T: Serialize>(value: &T) -> Result<Vec<u8>, StateError> {
    Ok(serde_json::to_vec(&VersionedRecord {
        version: RECORD_VERSION,
        value,
    })?)
}

fn list_prefix(prefix: &str) -> String {
    format!("{}/", prefix.trim_end_matches('/'))
}

fn backend_error(error: impl Display) -> StateError {
    StateError::Backend(error.to_string())
}

/// Raw bytes of one VM's manifest and placement records, read atomically.
#[derive(Debug, Default)]
struct RawVmPair {
    manifest: Option<Vec<u8>>,
    placement: Option<Vec<u8>>,
}

/// Decodes one VM's records; both absent is `None`, a lone record is incomplete.
fn decode_pair<T: DeserializeOwned>(
    vmid: Ulid,
    raw: &RawVmPair,
) -> Result<Option<VmState<T>>, StateError> {
    match (&raw.manifest, &raw.placement) {
        (None, None) => Ok(None),
        (Some(manifest), Some(placement)) => {
            let placement: PlacementRecord = decode_record(placement)?;
            if placement.vmid != vmid {
                return Err(StateError::Incomplete);
            }
            Ok(Some(VmState {
                manifest: decode_record(manifest)?,
                placement,
            }))
        }
        _ => Err(StateError::Incomplete),
    }
}

fn pair_vm_state<T: DeserializeOwned>(entries: Vec<(String, Vec<u8>)>) -> VmStateSnapshot<T> {
    let manifest_prefix = list_prefix(VM_MANIFESTS_PREFIX);
    let placement_prefix = list_prefix(PLACEMENT_PREFIX);
    let mut raw = BTreeMap::<String, RawVmPair>::new();
    for (record_key, bytes) in entries {
        if let Some(id) = record_key.strip_prefix(&manifest_prefix) {
            raw.entry(id.to_owned()).or_default().manifest = Some(bytes);
        } else if let Some(id) = record_key.strip_prefix(&placement_prefix) {
            raw.entry(id.to_owned()).or_default().placement = Some(bytes);
        }
    }

    let mut snapshot = VmStateSnapshot {
        states: Vec::new(),
        invalid: Vec::new(),
    };
    for (id, pair) in raw {
        let decoded = id
            .parse::<Ulid>()
            .map_err(|_| StateError::Incomplete)
            .and_then(|vmid| decode_pair(vmid, &pair));
        match decoded {
            Ok(Some(state)) => snapshot.states.push(state),
            Ok(None) => {}
            Err(error) => snapshot.invalid.push(InvalidVmState { id, error }),
        }
    }
    snapshot
}

/// Outcome of validating a stop request against the current records.
enum StopPlan {
    /// The requested marker is already durable.
    AlreadyMarked(PlacementRecord),
    /// Write `encoded` (the marked placement) if the records are unchanged.
    Mark {
        placement: PlacementRecord,
        encoded: Vec<u8>,
    },
}

fn plan_stop(vmid: Ulid, intent: StopIntent, raw: &RawVmPair) -> Result<StopPlan, StateError> {
    let Some(mut placement) = decode_pair::<serde_json::Value>(vmid, raw)?.map(|s| s.placement)
    else {
        return Err(StateError::Missing);
    };
    if placement.lifecycle == intent.lifecycle() {
        return Ok(StopPlan::AlreadyMarked(placement));
    }
    if placement.lifecycle != PlacementLifecycle::Active {
        return Err(StateError::Conflict);
    }
    placement.lifecycle = intent.lifecycle();
    let encoded = encode_record(&placement)?;
    Ok(StopPlan::Mark { placement, encoded })
}

/// Returns `true` when the exact expected pair is present and may be deleted,
/// or `false` when a prior finalization already removed both records.
fn plan_complete(expected: &PlacementRecord, raw: &RawVmPair) -> Result<bool, StateError> {
    match decode_pair::<serde_json::Value>(expected.vmid, raw)? {
        None => Ok(false),
        Some(state) if state.placement == *expected => Ok(true),
        Some(_) => Err(StateError::Conflict),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreHealth {
    pub healthy: bool,
    pub message: String,
}

#[async_trait]
pub trait ClusterStateStore: Send + Sync {
    async fn put<T: Serialize + Send + Sync>(&self, key: &str, value: &T)
    -> Result<(), StateError>;
    async fn get<T: DeserializeOwned + Send>(&self, key: &str) -> Result<Option<T>, StateError>;
    async fn list<T: DeserializeOwned + Send>(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, T)>, StateError>;
    async fn delete(&self, key: &str) -> Result<(), StateError>;
    async fn health(&self) -> StoreHealth;
}

pub fn key<T: Display + ?Sized>(prefix: &str, id: &T) -> String {
    format!("{prefix}/{id}")
}

#[derive(Clone)]
pub struct EtcdStateStore {
    /// Boxed behind an `Arc` to keep `StateStore` small; each call clones the
    /// cheap underlying handle because etcd-client methods take `&mut self`.
    client: Arc<Client>,
}

impl EtcdStateStore {
    pub async fn connect(
        endpoints: &[String],
        username: Option<&str>,
        password: Option<&str>,
        tls: Option<TlsConfig>,
        timeout: Duration,
        retries: u32,
    ) -> Result<Self, StateError> {
        let mut options = ConnectOptions::default()
            .with_timeout(timeout)
            .with_connect_timeout(timeout);
        if let (Some(user), Some(pass)) = (username, password) {
            options = options.with_user(user, pass);
        }
        if let Some(tls) = tls {
            let ca = std::fs::read(&tls.ca_file)
                .map_err(|error| StateError::Backend(format!("read etcd CA file: {error}")))?;
            options = options.with_tls(TlsOptions::new().ca_certificate(Certificate::from_pem(ca)));
        }

        let attempts = retries.max(1);
        let mut last_error = None;
        for attempt in 0..attempts {
            let result = match Client::connect(endpoints, Some(options.clone())).await {
                Ok(mut client) => client.status().await.map(|_| client),
                Err(error) => Err(error),
            };
            match result {
                Ok(client) => {
                    return Ok(Self {
                        client: Arc::new(client),
                    });
                }
                Err(error) => last_error = Some(error.to_string()),
            }
            let next_attempt = attempt.saturating_add(1);
            if next_attempt < attempts {
                let delay_ms = 100_u64.saturating_mul(u64::from(next_attempt));
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
        Err(StateError::Backend(format!(
            "unable to connect to etcd after {attempts} attempts: {}",
            last_error.unwrap_or_else(|| "unknown error".to_owned())
        )))
    }

    fn client(&self) -> Client {
        Client::clone(&self.client)
    }

    /// Runs read-only gets in one transaction so all results share a revision.
    async fn snapshot(
        &self,
        reads: Vec<(String, Option<GetOptions>)>,
    ) -> Result<Vec<Vec<(String, Vec<u8>)>>, StateError> {
        let ops: Vec<_> = reads
            .into_iter()
            .map(|(read_key, options)| TxnOp::get(read_key, options))
            .collect();
        let response = self
            .client()
            .txn(Txn::new().and_then(ops))
            .await
            .map_err(backend_error)?;
        response
            .op_responses()
            .into_iter()
            .map(|op| match op {
                TxnOpResponse::Get(get) => get
                    .kvs()
                    .iter()
                    .map(|kv| {
                        Ok((
                            kv.key_str().map_err(backend_error)?.to_owned(),
                            kv.value().to_vec(),
                        ))
                    })
                    .collect(),
                _ => Err(StateError::Backend(
                    "unexpected etcd transaction response".to_owned(),
                )),
            })
            .collect()
    }

    async fn read_vm_pair(&self, vmid: Ulid) -> Result<RawVmPair, StateError> {
        let mut results = self
            .snapshot(vec![
                (key(VM_MANIFESTS_PREFIX, &vmid), None),
                (key(PLACEMENT_PREFIX, &vmid), None),
            ])
            .await?
            .into_iter();
        let mut first_value = || {
            results
                .next()
                .and_then(|values| values.into_iter().next())
                .map(|(_, value)| value)
        };
        let manifest = first_value();
        let placement = first_value();
        Ok(RawVmPair {
            manifest,
            placement,
        })
    }

    async fn read_vm_entries(&self) -> Result<Vec<(String, Vec<u8>)>, StateError> {
        let prefix_read =
            |prefix: &str| (list_prefix(prefix), Some(GetOptions::new().with_prefix()));
        Ok(self
            .snapshot(vec![
                prefix_read(VM_MANIFESTS_PREFIX),
                prefix_read(PLACEMENT_PREFIX),
            ])
            .await?
            .into_iter()
            .flatten()
            .collect())
    }

    async fn create_vm_state(
        &self,
        manifest_key: &str,
        manifest: Vec<u8>,
        placement_key: &str,
        placement: Vec<u8>,
    ) -> Result<(), StateError> {
        let transaction = Txn::new()
            .when([
                Compare::version(manifest_key, CompareOp::Equal, 0),
                Compare::version(placement_key, CompareOp::Equal, 0),
            ])
            .and_then([
                TxnOp::put(manifest_key, manifest, None),
                TxnOp::put(placement_key, placement, None),
            ]);
        let response = self
            .client()
            .txn(transaction)
            .await
            .map_err(backend_error)?;
        if response.succeeded() {
            Ok(())
        } else {
            Err(StateError::AlreadyExists)
        }
    }

    async fn begin_vm_stop(
        &self,
        vmid: Ulid,
        intent: StopIntent,
    ) -> Result<PlacementRecord, StateError> {
        let manifest_key = key(VM_MANIFESTS_PREFIX, &vmid);
        let placement_key = key(PLACEMENT_PREFIX, &vmid);
        let raw = self.read_vm_pair(vmid).await?;
        let (placement, encoded) = match plan_stop(vmid, intent, &raw)? {
            StopPlan::AlreadyMarked(placement) => return Ok(placement),
            StopPlan::Mark { placement, encoded } => (placement, encoded),
        };
        let (Some(previous_manifest), Some(previous_placement)) = (raw.manifest, raw.placement)
        else {
            return Err(StateError::Incomplete);
        };
        let response = self
            .client()
            .txn(
                Txn::new()
                    .when([
                        Compare::value(
                            placement_key.as_str(),
                            CompareOp::Equal,
                            previous_placement,
                        ),
                        Compare::value(manifest_key.as_str(), CompareOp::Equal, previous_manifest),
                    ])
                    .and_then([TxnOp::put(placement_key.as_str(), encoded, None)]),
            )
            .await
            .map_err(backend_error)?;
        if response.succeeded() {
            return Ok(placement);
        }
        // Lost the race: accept only a concurrent request that set the same marker.
        match plan_stop(vmid, intent, &self.read_vm_pair(vmid).await?)? {
            StopPlan::AlreadyMarked(current) => Ok(current),
            StopPlan::Mark { .. } => Err(StateError::Conflict),
        }
    }

    async fn complete_vm_stop(&self, expected: &PlacementRecord) -> Result<(), StateError> {
        let manifest_key = key(VM_MANIFESTS_PREFIX, &expected.vmid);
        let placement_key = key(PLACEMENT_PREFIX, &expected.vmid);
        let raw = self.read_vm_pair(expected.vmid).await?;
        if !plan_complete(expected, &raw)? {
            return Ok(());
        }
        let (Some(manifest), Some(placement)) = (raw.manifest, raw.placement) else {
            return Err(StateError::Incomplete);
        };
        let response = self
            .client()
            .txn(
                Txn::new()
                    .when([
                        Compare::value(placement_key.as_str(), CompareOp::Equal, placement),
                        Compare::value(manifest_key.as_str(), CompareOp::Equal, manifest),
                    ])
                    .and_then([
                        TxnOp::delete(manifest_key.as_str(), None),
                        TxnOp::delete(placement_key.as_str(), None),
                    ]),
            )
            .await
            .map_err(backend_error)?;
        if response.succeeded() {
            Ok(())
        } else {
            Err(StateError::Conflict)
        }
    }
}

#[derive(Clone)]
pub enum StateStore {
    Etcd(EtcdStateStore),
    Memory(MemoryStateStore),
}

impl StateStore {
    pub async fn connect(
        endpoints: &[String],
        username: Option<&str>,
        password: Option<&str>,
        tls: Option<TlsConfig>,
        timeout: Duration,
        retries: u32,
    ) -> Result<Self, StateError> {
        Ok(Self::Etcd(
            EtcdStateStore::connect(endpoints, username, password, tls, timeout, retries).await?,
        ))
    }

    /// Reads all manifests and placements from one etcd revision (or one memory
    /// read lock). Orphaned or undecodable pairs are reported in
    /// [`VmStateSnapshot::invalid`] rather than failing the whole read.
    pub async fn list_vm_state<T: DeserializeOwned + Send>(
        &self,
    ) -> Result<VmStateSnapshot<T>, StateError> {
        let entries = match self {
            Self::Etcd(store) => store.read_vm_entries().await?,
            Self::Memory(store) => store.read_vm_entries().await,
        };
        Ok(pair_vm_state(entries))
    }

    /// Reads only this VM's two records, atomically.
    pub async fn get_vm_state<T: DeserializeOwned + Send>(
        &self,
        vmid: Ulid,
    ) -> Result<Option<VmState<T>>, StateError> {
        let raw = match self {
            Self::Etcd(store) => store.read_vm_pair(vmid).await?,
            Self::Memory(store) => store.read_vm_pair(vmid).await,
        };
        decode_pair(vmid, &raw)
    }

    /// Atomically creates desired state; an identical active pair is an
    /// idempotent retry, while conflicts and incomplete pairs are rejected.
    pub async fn create_vm_state<T: Serialize + DeserializeOwned + PartialEq + Send + Sync>(
        &self,
        vmid: Ulid,
        manifest: &T,
        placement: &PlacementRecord,
    ) -> Result<(), StateError> {
        if placement.vmid != vmid || placement.lifecycle != PlacementLifecycle::Active {
            return Err(StateError::Conflict);
        }
        let is_same = |existing: &VmState<T>| {
            existing.manifest == *manifest && existing.placement == *placement
        };
        if let Some(existing) = self.get_vm_state::<T>(vmid).await? {
            return if is_same(&existing) {
                Ok(())
            } else {
                Err(StateError::AlreadyExists)
            };
        }

        let manifest_key = key(VM_MANIFESTS_PREFIX, &vmid);
        let placement_key = key(PLACEMENT_PREFIX, &vmid);
        let manifest_bytes = encode_record(manifest)?;
        let placement_bytes = encode_record(placement)?;

        let result = match self {
            Self::Etcd(store) => {
                store
                    .create_vm_state(
                        &manifest_key,
                        manifest_bytes,
                        &placement_key,
                        placement_bytes,
                    )
                    .await
            }
            Self::Memory(store) => {
                let mut values = store.values.write().await;
                if values.contains_key(&manifest_key) || values.contains_key(&placement_key) {
                    Err(StateError::AlreadyExists)
                } else {
                    values.insert(manifest_key, manifest_bytes);
                    values.insert(placement_key, placement_bytes);
                    drop(values);
                    Ok(())
                }
            }
        };
        if matches!(result, Err(StateError::AlreadyExists))
            && let Some(existing) = self.get_vm_state::<T>(vmid).await?
            && is_same(&existing)
        {
            return Ok(());
        }
        result
    }

    /// Persists the stop marker before runtime teardown is requested.
    pub async fn begin_vm_stop(
        &self,
        vmid: Ulid,
        intent: StopIntent,
    ) -> Result<PlacementRecord, StateError> {
        match self {
            Self::Etcd(store) => store.begin_vm_stop(vmid, intent).await,
            Self::Memory(store) => {
                let mut values = store.values.write().await;
                let raw = MemoryStateStore::pair_from(&values, vmid);
                let placement = match plan_stop(vmid, intent, &raw)? {
                    StopPlan::AlreadyMarked(placement) => placement,
                    StopPlan::Mark { placement, encoded } => {
                        values.insert(key(PLACEMENT_PREFIX, &vmid), encoded);
                        placement
                    }
                };
                drop(values);
                Ok(placement)
            }
        }
    }

    /// Deletes both records only if the exact marked placement is still current.
    /// Finalizing an already-absent pair succeeds; an incomplete pair is retained.
    pub async fn complete_vm_stop(&self, expected: &PlacementRecord) -> Result<(), StateError> {
        if expected.lifecycle == PlacementLifecycle::Active {
            return Err(StateError::Conflict);
        }
        match self {
            Self::Etcd(store) => store.complete_vm_stop(expected).await,
            Self::Memory(store) => {
                let mut values = store.values.write().await;
                let raw = MemoryStateStore::pair_from(&values, expected.vmid);
                if plan_complete(expected, &raw)? {
                    values.remove(&key(PLACEMENT_PREFIX, &expected.vmid));
                    values.remove(&key(VM_MANIFESTS_PREFIX, &expected.vmid));
                }
                drop(values);
                Ok(())
            }
        }
    }
}

#[async_trait]
impl ClusterStateStore for StateStore {
    async fn put<T: Serialize + Send + Sync>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), StateError> {
        match self {
            Self::Etcd(store) => store.put(key, value).await,
            Self::Memory(store) => store.put(key, value).await,
        }
    }
    async fn get<T: DeserializeOwned + Send>(&self, key: &str) -> Result<Option<T>, StateError> {
        match self {
            Self::Etcd(store) => store.get(key).await,
            Self::Memory(store) => store.get(key).await,
        }
    }
    async fn list<T: DeserializeOwned + Send>(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, T)>, StateError> {
        match self {
            Self::Etcd(store) => store.list(prefix).await,
            Self::Memory(store) => store.list(prefix).await,
        }
    }
    async fn delete(&self, key: &str) -> Result<(), StateError> {
        match self {
            Self::Etcd(store) => store.delete(key).await,
            Self::Memory(store) => store.delete(key).await,
        }
    }
    async fn health(&self) -> StoreHealth {
        match self {
            Self::Etcd(store) => store.health().await,
            Self::Memory(store) => store.health().await,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub ca_file: String,
}

#[async_trait]
impl ClusterStateStore for EtcdStateStore {
    async fn put<T: Serialize + Send + Sync>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), StateError> {
        self.client()
            .put(key, encode_record(value)?, None)
            .await
            .map(|_| ())
            .map_err(backend_error)
    }

    async fn get<T: DeserializeOwned + Send>(&self, key: &str) -> Result<Option<T>, StateError> {
        let response = self.client().get(key, None).await.map_err(backend_error)?;
        let Some(value) = response.kvs().first() else {
            return Ok(None);
        };
        Ok(Some(decode_record(value.value())?))
    }

    async fn list<T: DeserializeOwned + Send>(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, T)>, StateError> {
        let prefix = list_prefix(prefix);
        let response = self
            .client()
            .get(prefix.as_str(), Some(GetOptions::new().with_prefix()))
            .await
            .map_err(backend_error)?;
        response
            .kvs()
            .iter()
            .map(|value| {
                Ok((
                    value.key_str().map_err(backend_error)?.to_owned(),
                    decode_record(value.value())?,
                ))
            })
            .collect()
    }

    async fn delete(&self, key: &str) -> Result<(), StateError> {
        self.client()
            .delete(key, None)
            .await
            .map(|_| ())
            .map_err(backend_error)
    }

    async fn health(&self) -> StoreHealth {
        match self.client().status().await {
            Ok(_) => StoreHealth {
                healthy: true,
                message: "etcd is reachable".to_owned(),
            },
            Err(error) => StoreHealth {
                healthy: false,
                message: format!("etcd health check failed: {error}"),
            },
        }
    }
}

#[derive(Clone, Default)]
pub struct MemoryStateStore {
    values: Arc<RwLock<BTreeMap<String, Vec<u8>>>>,
}

impl MemoryStateStore {
    fn pair_from(values: &BTreeMap<String, Vec<u8>>, vmid: Ulid) -> RawVmPair {
        RawVmPair {
            manifest: values.get(&key(VM_MANIFESTS_PREFIX, &vmid)).cloned(),
            placement: values.get(&key(PLACEMENT_PREFIX, &vmid)).cloned(),
        }
    }

    async fn read_vm_pair(&self, vmid: Ulid) -> RawVmPair {
        Self::pair_from(&*self.values.read().await, vmid)
    }

    async fn read_vm_entries(&self) -> Vec<(String, Vec<u8>)> {
        let manifest_prefix = list_prefix(VM_MANIFESTS_PREFIX);
        let placement_prefix = list_prefix(PLACEMENT_PREFIX);
        self.values
            .read()
            .await
            .iter()
            .filter(|(record_key, _)| {
                record_key.starts_with(&manifest_prefix)
                    || record_key.starts_with(&placement_prefix)
            })
            .map(|(record_key, value)| (record_key.clone(), value.clone()))
            .collect()
    }
}

#[async_trait]
impl ClusterStateStore for MemoryStateStore {
    async fn put<T: Serialize + Send + Sync>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), StateError> {
        let record = encode_record(value)?;
        self.values.write().await.insert(key.to_owned(), record);
        Ok(())
    }
    async fn get<T: DeserializeOwned + Send>(&self, key: &str) -> Result<Option<T>, StateError> {
        let value = self.values.read().await.get(key).cloned();
        let Some(value) = value else {
            return Ok(None);
        };
        Ok(Some(decode_record(&value)?))
    }
    async fn list<T: DeserializeOwned + Send>(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, T)>, StateError> {
        let prefix = list_prefix(prefix);
        self.values
            .read()
            .await
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, value)| Ok((key.clone(), decode_record(value)?)))
            .collect()
    }
    async fn delete(&self, key: &str) -> Result<(), StateError> {
        self.values.write().await.remove(key);
        Ok(())
    }
    async fn health(&self) -> StoreHealth {
        StoreHealth {
            healthy: true,
            message: "memory store is healthy".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClusterStateStore, MemoryStateStore, NODE_STATE_PREFIX, NodeStateRecord, OPERATIONS_PREFIX,
        OperationRecord, OperationState, PLACEMENT_PREFIX, PlacementLifecycle, PlacementRecord,
        StateError, StateStore, StopIntent, VM_MANIFESTS_PREFIX, key,
    };
    use std::{collections::BTreeMap, time::Duration};
    use ulid::Ulid;

    /// Set to a comma-separated endpoint list (for example `http://127.0.0.1:2379`)
    /// to run the paired-state scenarios against a real etcd as well as memory.
    const ETCD_TEST_ENDPOINTS: &str = "ODOROBO_TEST_ETCD_ENDPOINTS";

    fn memory_store() -> StateStore {
        StateStore::Memory(MemoryStateStore::default())
    }

    async fn etcd_store() -> Option<StateStore> {
        let endpoints: Vec<String> = std::env::var(ETCD_TEST_ENDPOINTS)
            .ok()?
            .split(',')
            .map(str::to_owned)
            .collect();
        Some(
            StateStore::connect(&endpoints, None, None, None, Duration::from_secs(5), 3)
                .await
                .expect("connect to test etcd"),
        )
    }

    /// Runs a scenario against memory and, when configured, etcd. Scenarios use
    /// fresh VM IDs and clean up after themselves so a shared etcd is safe.
    macro_rules! store_scenario {
        ($name:ident, $scenario:ident) => {
            mod $name {
                #[tokio::test]
                async fn memory() {
                    super::$scenario(&super::memory_store()).await;
                }

                #[tokio::test]
                async fn etcd() {
                    if let Some(store) = super::etcd_store().await {
                        super::$scenario(&store).await;
                    }
                }
            }
        };
    }

    fn placement(vmid: Ulid) -> PlacementRecord {
        PlacementRecord {
            vmid,
            node: "manager-owned-node".to_owned(),
            generation: Some(Ulid::generate()),
            lifecycle: PlacementLifecycle::Active,
        }
    }

    async fn create_pair(store: &StateStore, vmid: Ulid) -> (serde_json::Value, PlacementRecord) {
        let manifest = serde_json::json!({"id": vmid, "name": "demo"});
        let placement = placement(vmid);
        store
            .create_vm_state(vmid, &manifest, &placement)
            .await
            .unwrap();
        (manifest, placement)
    }

    async fn remove_pair(store: &StateStore, vmid: Ulid) {
        store
            .delete(&key(VM_MANIFESTS_PREFIX, &vmid))
            .await
            .unwrap();
        store.delete(&key(PLACEMENT_PREFIX, &vmid)).await.unwrap();
    }

    async fn paired_create_is_atomic_idempotent_and_conflict_safe(store: &StateStore) {
        let vmid = Ulid::generate();
        let (manifest, placement) = create_pair(store, vmid).await;
        store
            .create_vm_state(vmid, &manifest, &placement)
            .await
            .unwrap();
        assert!(matches!(
            store
                .create_vm_state(vmid, &serde_json::json!({"other": true}), &placement)
                .await,
            Err(StateError::AlreadyExists)
        ));
        let state = store
            .get_vm_state::<serde_json::Value>(vmid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.manifest, manifest);
        assert_eq!(state.placement, placement);
        remove_pair(store, vmid).await;
    }
    store_scenario!(
        paired_create,
        paired_create_is_atomic_idempotent_and_conflict_safe
    );

    async fn orphan_is_reported_without_hiding_other_vms(store: &StateStore) {
        let orphan = Ulid::generate();
        let healthy = Ulid::generate();
        let (_, healthy_placement) = create_pair(store, healthy).await;
        store
            .put(
                &key(VM_MANIFESTS_PREFIX, &orphan),
                &serde_json::json!({"id": orphan}),
            )
            .await
            .unwrap();

        let snapshot = store.list_vm_state::<serde_json::Value>().await.unwrap();
        assert!(
            snapshot
                .states
                .iter()
                .any(|state| state.placement == healthy_placement)
        );
        assert!(snapshot.invalid.iter().any(|invalid| {
            invalid.id == orphan.to_string() && matches!(invalid.error, StateError::Incomplete)
        }));
        assert!(matches!(
            store.get_vm_state::<serde_json::Value>(orphan).await,
            Err(StateError::Incomplete)
        ));
        // Operations on other VMs are unaffected by the orphan.
        store
            .begin_vm_stop(healthy, StopIntent::Delete)
            .await
            .unwrap();
        assert!(matches!(
            store.begin_vm_stop(orphan, StopIntent::Delete).await,
            Err(StateError::Incomplete)
        ));

        let placement = placement(orphan);
        store
            .put(&key(PLACEMENT_PREFIX, &orphan), &placement)
            .await
            .unwrap();
        let state = store
            .get_vm_state::<serde_json::Value>(orphan)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.placement, placement);
        remove_pair(store, orphan).await;
        remove_pair(store, healthy).await;
    }
    store_scenario!(
        orphan_reporting,
        orphan_is_reported_without_hiding_other_vms
    );

    async fn placement_for_another_vm_is_rejected(store: &StateStore) {
        let vmid = Ulid::generate();
        store
            .put(&key(VM_MANIFESTS_PREFIX, &vmid), &serde_json::json!({}))
            .await
            .unwrap();
        store
            .put(&key(PLACEMENT_PREFIX, &vmid), &placement(Ulid::generate()))
            .await
            .unwrap();
        assert!(matches!(
            store.begin_vm_stop(vmid, StopIntent::Delete).await,
            Err(StateError::Incomplete)
        ));
        remove_pair(store, vmid).await;
    }
    store_scenario!(mismatched_placement, placement_for_another_vm_is_rejected);

    async fn stop_marker_is_durable_and_preserves_manager_owner_and_generation(store: &StateStore) {
        let vmid = Ulid::generate();
        let (manifest, original) = create_pair(store, vmid).await;
        let stopping = store.begin_vm_stop(vmid, StopIntent::Delete).await.unwrap();
        assert_eq!(stopping.node, original.node);
        assert_eq!(stopping.generation, original.generation);
        assert_eq!(stopping.lifecycle, PlacementLifecycle::Deleting);
        assert_eq!(
            store.begin_vm_stop(vmid, StopIntent::Delete).await.unwrap(),
            stopping
        );
        assert!(matches!(
            store.begin_vm_stop(vmid, StopIntent::Shutdown).await,
            Err(StateError::Conflict)
        ));
        let state = store
            .get_vm_state::<serde_json::Value>(vmid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.manifest, manifest);
        assert_eq!(state.placement, stopping);
        assert!(matches!(
            store.create_vm_state(vmid, &manifest, &original).await,
            Err(StateError::AlreadyExists)
        ));
        assert!(matches!(
            store
                .begin_vm_stop(Ulid::generate(), StopIntent::Delete)
                .await,
            Err(StateError::Missing)
        ));
        remove_pair(store, vmid).await;
    }
    store_scenario!(
        stop_marker,
        stop_marker_is_durable_and_preserves_manager_owner_and_generation
    );

    async fn completion_compare_rejects_stale_generation_and_retries_exactly(store: &StateStore) {
        let vmid = Ulid::generate();
        create_pair(store, vmid).await;
        let expected = store.begin_vm_stop(vmid, StopIntent::Delete).await.unwrap();
        let mut stale = expected.clone();
        stale.generation = Some(Ulid::generate());
        assert!(matches!(
            store.complete_vm_stop(&stale).await,
            Err(StateError::Conflict)
        ));
        assert!(
            store
                .get_vm_state::<serde_json::Value>(vmid)
                .await
                .unwrap()
                .is_some()
        );
        store.complete_vm_stop(&expected).await.unwrap();
        store.complete_vm_stop(&expected).await.unwrap();
        assert!(
            store
                .get_vm_state::<serde_json::Value>(vmid)
                .await
                .unwrap()
                .is_none()
        );
    }
    store_scenario!(
        completion_compare,
        completion_compare_rejects_stale_generation_and_retries_exactly
    );

    async fn completion_rejects_incomplete_pairs_without_deleting_the_remaining_record(
        store: &StateStore,
    ) {
        let vmid = Ulid::generate();
        create_pair(store, vmid).await;
        let expected = store.begin_vm_stop(vmid, StopIntent::Delete).await.unwrap();
        let placement_key = key(PLACEMENT_PREFIX, &vmid);
        store
            .delete(&key(VM_MANIFESTS_PREFIX, &vmid))
            .await
            .unwrap();

        assert!(matches!(
            store.complete_vm_stop(&expected).await,
            Err(StateError::Incomplete)
        ));
        assert!(
            store
                .get::<PlacementRecord>(&placement_key)
                .await
                .unwrap()
                .is_some()
        );
        remove_pair(store, vmid).await;
    }
    store_scenario!(
        incomplete_completion,
        completion_rejects_incomplete_pairs_without_deleting_the_remaining_record
    );

    #[tokio::test]
    async fn versioned_crud_is_non_destructive_and_rejects_future_versions() {
        let store = MemoryStateStore::default();
        let record_key = key(VM_MANIFESTS_PREFIX, &"vm-1");
        store
            .put(&record_key, &serde_json::json!({"name": "demo"}))
            .await
            .unwrap();
        assert_eq!(
            store.get::<serde_json::Value>(&record_key).await.unwrap(),
            Some(serde_json::json!({"name": "demo"}))
        );
        assert_eq!(
            store
                .list::<serde_json::Value>(VM_MANIFESTS_PREFIX)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .get::<serde_json::Value>("missing")
                .await
                .unwrap()
                .is_none()
        );
        store.values.write().await.insert(
            record_key.clone(),
            serde_json::to_vec(&serde_json::json!({"version": 2, "value": {}})).unwrap(),
        );
        assert!(matches!(
            store.get::<serde_json::Value>(&record_key).await,
            Err(StateError::UnsupportedVersion(2))
        ));
        assert!(store.values.read().await.contains_key(&record_key));
        store.delete(&record_key).await.unwrap();
        assert!(
            store
                .get::<serde_json::Value>(&record_key)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn prefix_listing_excludes_similarly_named_keys() {
        let store = MemoryStateStore::default();
        store
            .put(&key(PLACEMENT_PREFIX, &"vm-1"), &serde_json::json!(1))
            .await
            .unwrap();
        store
            .put(
                &format!("{PLACEMENT_PREFIX}-backup/vm-2"),
                &serde_json::json!(2),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .list::<serde_json::Value>(PLACEMENT_PREFIX)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn reserved_v1_node_and_operation_payloads_roundtrip_in_memory() {
        let store = memory_store();
        let node = NodeStateRecord {
            node: "node-a".to_owned(),
            metadata: crate::types::ObjectMetadata {
                labels: BTreeMap::from([("zone".to_owned(), "west".to_owned())]),
                annotations: BTreeMap::from([("owner".to_owned(), "platform".to_owned())]),
            },
        };
        let operation = OperationRecord {
            operation_id: Ulid::generate(),
            kind: "vm.delete".to_owned(),
            target: "vm-123".to_owned(),
            state: OperationState::Running,
        };
        let node_key = key(NODE_STATE_PREFIX, "node-a");
        let operation_key = key(OPERATIONS_PREFIX, &operation.operation_id);
        store.put(&node_key, &node).await.unwrap();
        store.put(&operation_key, &operation).await.unwrap();

        let decoded_node = store
            .get::<NodeStateRecord>(&node_key)
            .await
            .unwrap()
            .unwrap();
        let decoded_operation = store
            .get::<OperationRecord>(&operation_key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decoded_node.node, node.node);
        assert_eq!(decoded_node.metadata.labels, node.metadata.labels);
        assert_eq!(decoded_node.metadata.annotations, node.metadata.annotations);
        assert_eq!(decoded_operation, operation);
        drop(store);
    }

    #[test]
    fn legacy_placement_defaults_to_active_without_a_generation() {
        let placement: PlacementRecord = serde_json::from_value(serde_json::json!({
            "vmid": Ulid::generate(), "node": "legacy-node"
        }))
        .unwrap();
        assert_eq!(placement.lifecycle, PlacementLifecycle::Active);
        assert_eq!(placement.generation, None);
    }
}
