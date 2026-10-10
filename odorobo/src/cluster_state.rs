//! Durable desired cluster state.
//!
//! Keys are versioned and namespaced as `/odorobo/v1/{kind}/{id}`. Values are
//! JSON records with a `version` field so readers can reject unknown versions
//! rather than silently misinterpreting state.

use std::{collections::BTreeMap, fmt::Display, sync::Arc, time::Duration};

use async_trait::async_trait;
use etcd_client::{
    Certificate, Client, Compare, CompareOp, ConnectOptions, TlsOptions, Txn, TxnOp,
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

fn list_prefix(prefix: &str) -> String {
    format!("{}/", prefix.trim_end_matches('/'))
}

fn pair_vm_state<T: DeserializeOwned>(
    entries: Vec<(String, Vec<u8>)>,
) -> Result<Vec<VmState<T>>, StateError> {
    let mut records = BTreeMap::<Ulid, (Option<T>, Option<PlacementRecord>)>::new();
    for (record_key, bytes) in entries {
        if let Some(id) = record_key.strip_prefix(&list_prefix(VM_MANIFESTS_PREFIX)) {
            let vmid = id.parse().map_err(|_| StateError::Incomplete)?;
            records.entry(vmid).or_default().0 = Some(decode_record(&bytes)?);
        } else if let Some(id) = record_key.strip_prefix(&list_prefix(PLACEMENT_PREFIX)) {
            let vmid: Ulid = id.parse().map_err(|_| StateError::Incomplete)?;
            let placement: PlacementRecord = decode_record(&bytes)?;
            if placement.vmid != vmid {
                return Err(StateError::Incomplete);
            }
            records.entry(vmid).or_default().1 = Some(placement);
        }
    }
    records
        .into_values()
        .map(|(manifest, placement)| match (manifest, placement) {
            (Some(manifest), Some(placement)) => Ok(VmState {
                manifest,
                placement,
            }),
            _ => Err(StateError::Incomplete),
        })
        .collect()
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

    /// Reads manifests and placements from one etcd range read (or one memory
    /// read lock), rejecting orphaned or mismatched records.
    pub async fn list_vm_state<T: DeserializeOwned + Send>(
        &self,
    ) -> Result<Vec<VmState<T>>, StateError> {
        let entries = match self {
            Self::Etcd(store) => store.read_vm_entries().await?,
            Self::Memory(store) => store
                .values
                .read()
                .await
                .iter()
                .filter(|(record_key, _)| {
                    record_key.starts_with(&list_prefix(VM_MANIFESTS_PREFIX))
                        || record_key.starts_with(&list_prefix(PLACEMENT_PREFIX))
                })
                .map(|(record_key, value)| (record_key.clone(), value.clone()))
                .collect(),
        };
        pair_vm_state(entries)
    }

    pub async fn get_vm_state<T: DeserializeOwned + Send>(
        &self,
        vmid: Ulid,
    ) -> Result<Option<VmState<T>>, StateError> {
        Ok(self
            .list_vm_state::<T>()
            .await?
            .into_iter()
            .find(|state| state.placement.vmid == vmid))
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
        if let Some(existing) = self.get_vm_state::<T>(vmid).await? {
            return if existing.manifest == *manifest && existing.placement == *placement {
                Ok(())
            } else {
                Err(StateError::AlreadyExists)
            };
        }

        let manifest_key = key(VM_MANIFESTS_PREFIX, &vmid);
        let placement_key = key(PLACEMENT_PREFIX, &vmid);
        let manifest_bytes = serde_json::to_vec(&VersionedRecord {
            version: RECORD_VERSION,
            value: manifest,
        })?;
        let placement_bytes = serde_json::to_vec(&VersionedRecord {
            version: RECORD_VERSION,
            value: placement,
        })?;

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
            && existing.manifest == *manifest
            && existing.placement == *placement
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
        self.get_vm_state::<serde_json::Value>(vmid)
            .await?
            .ok_or(StateError::Missing)?;
        let placement_key = key(PLACEMENT_PREFIX, &vmid);
        let manifest_key = key(VM_MANIFESTS_PREFIX, &vmid);
        match self {
            Self::Memory(store) => {
                let mut values = store.values.write().await;
                let Some(bytes) = values.get(&placement_key) else {
                    return if values.contains_key(&manifest_key) {
                        Err(StateError::Incomplete)
                    } else {
                        Err(StateError::Missing)
                    };
                };
                if !values.contains_key(&manifest_key) {
                    return Err(StateError::Incomplete);
                }
                let mut placement: PlacementRecord = decode_record(bytes)?;
                if placement.vmid != vmid {
                    return Err(StateError::Incomplete);
                }
                if placement.lifecycle == intent.lifecycle() {
                    return Ok(placement);
                }
                if placement.lifecycle != PlacementLifecycle::Active {
                    return Err(StateError::Conflict);
                }
                placement.lifecycle = intent.lifecycle();
                values.insert(
                    placement_key,
                    serde_json::to_vec(&VersionedRecord {
                        version: RECORD_VERSION,
                        value: &placement,
                    })?,
                );
                drop(values);
                Ok(placement)
            }
            Self::Etcd(store) => store.begin_vm_stop(&placement_key, intent).await,
        }
    }

    /// Deletes both records only if the exact marked placement is still current.
    pub async fn complete_vm_stop(&self, expected: &PlacementRecord) -> Result<(), StateError> {
        if expected.lifecycle == PlacementLifecycle::Active {
            return Err(StateError::Conflict);
        }
        let manifest_key = key(VM_MANIFESTS_PREFIX, &expected.vmid);
        let placement_key = key(PLACEMENT_PREFIX, &expected.vmid);
        match self {
            Self::Etcd(store) => {
                store
                    .complete_vm_stop(&manifest_key, &placement_key, expected)
                    .await
            }
            Self::Memory(store) => {
                let mut values = store.values.write().await;
                let Some(encoded) = values.get(&placement_key) else {
                    return if values.contains_key(&manifest_key) {
                        Err(StateError::Incomplete)
                    } else {
                        Ok(())
                    };
                };
                let placement: PlacementRecord = decode_record(encoded)?;
                if !values.contains_key(&manifest_key) {
                    return Err(StateError::Incomplete);
                }
                if placement != *expected {
                    return Err(StateError::Conflict);
                }
                values.remove(&placement_key);
                values.remove(&manifest_key);
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
        let record = serde_json::to_vec(&VersionedRecord {
            version: RECORD_VERSION,
            value,
        })?;
        self.client
            .as_ref()
            .clone()
            .put(key, record, None)
            .await
            .map(|_| ())
            .map_err(|error| StateError::Backend(error.to_string()))
    }

    async fn get<T: DeserializeOwned + Send>(&self, key: &str) -> Result<Option<T>, StateError> {
        let response = self
            .client
            .as_ref()
            .clone()
            .get(key, None)
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
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
            .client
            .as_ref()
            .clone()
            .get(
                prefix.as_str(),
                Some(etcd_client::GetOptions::new().with_prefix()),
            )
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        response
            .kvs()
            .iter()
            .map(|value| {
                Ok((
                    value.key_str().unwrap_or_default().to_owned(),
                    decode_record(value.value())?,
                ))
            })
            .collect()
    }

    async fn delete(&self, key: &str) -> Result<(), StateError> {
        self.client
            .as_ref()
            .clone()
            .delete(key, None)
            .await
            .map(|_| ())
            .map_err(|error| StateError::Backend(error.to_string()))
    }

    async fn health(&self) -> StoreHealth {
        match self.client.as_ref().clone().status().await {
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

impl EtcdStateStore {
    async fn read_vm_entries(&self) -> Result<Vec<(String, Vec<u8>)>, StateError> {
        let prefix = list_prefix(KEY_PREFIX);
        let response = self
            .client
            .as_ref()
            .clone()
            .get(
                prefix.as_str(),
                Some(etcd_client::GetOptions::new().with_prefix()),
            )
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        let mut entries = Vec::new();
        for value in response.kvs() {
            let record_key = value
                .key_str()
                .map_err(|error| StateError::Backend(error.to_string()))?;
            if record_key.starts_with(&list_prefix(VM_MANIFESTS_PREFIX))
                || record_key.starts_with(&list_prefix(PLACEMENT_PREFIX))
            {
                entries.push((record_key.to_owned(), value.value().to_vec()));
            }
        }
        Ok(entries)
    }

    async fn begin_vm_stop(
        &self,
        placement_key: &str,
        intent: StopIntent,
    ) -> Result<PlacementRecord, StateError> {
        let response = self
            .client
            .as_ref()
            .clone()
            .get(placement_key, None)
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        let value = response.kvs().first().ok_or(StateError::Missing)?;
        let previous = value.value().to_vec();
        let mut placement: PlacementRecord = decode_record(&previous)?;
        let manifest_key = key(VM_MANIFESTS_PREFIX, &placement.vmid);
        if self
            .client
            .as_ref()
            .clone()
            .get(manifest_key, None)
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?
            .kvs()
            .is_empty()
        {
            return Err(StateError::Incomplete);
        }
        if placement.lifecycle == intent.lifecycle() {
            return Ok(placement);
        }
        if placement.lifecycle != PlacementLifecycle::Active {
            return Err(StateError::Conflict);
        }
        placement.lifecycle = intent.lifecycle();
        let encoded = serde_json::to_vec(&VersionedRecord {
            version: RECORD_VERSION,
            value: &placement,
        })?;
        let response = self
            .client
            .as_ref()
            .clone()
            .txn(
                Txn::new()
                    .when([Compare::value(placement_key, CompareOp::Equal, previous)])
                    .and_then([TxnOp::put(placement_key, encoded, None)]),
            )
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        if response.succeeded() {
            return Ok(placement);
        }
        let current = self
            .client
            .as_ref()
            .clone()
            .get(placement_key, None)
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        let Some(value) = current.kvs().first() else {
            return Err(StateError::Missing);
        };
        let current: PlacementRecord = decode_record(value.value())?;
        if current.lifecycle == intent.lifecycle() {
            Ok(current)
        } else {
            Err(StateError::Conflict)
        }
    }

    async fn complete_vm_stop(
        &self,
        manifest_key: &str,
        placement_key: &str,
        expected: &PlacementRecord,
    ) -> Result<(), StateError> {
        let response = self
            .client
            .as_ref()
            .clone()
            .get(placement_key, None)
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        let Some(value) = response.kvs().first() else {
            let manifest = self
                .client
                .as_ref()
                .clone()
                .get(manifest_key, None)
                .await
                .map_err(|error| StateError::Backend(error.to_string()))?;
            return if manifest.kvs().is_empty() {
                Ok(())
            } else {
                Err(StateError::Incomplete)
            };
        };
        let encoded = value.value().to_vec();
        let current: PlacementRecord = decode_record(&encoded)?;
        if current != *expected {
            return Err(StateError::Conflict);
        }
        let manifest = self
            .client
            .as_ref()
            .clone()
            .get(manifest_key, None)
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        let Some(manifest) = manifest.kvs().first() else {
            return Err(StateError::Incomplete);
        };
        let manifest = manifest.value().to_vec();
        let response = self
            .client
            .as_ref()
            .clone()
            .txn(
                Txn::new()
                    .when([
                        Compare::value(placement_key, CompareOp::Equal, encoded),
                        Compare::value(manifest_key, CompareOp::Equal, manifest),
                    ])
                    .and_then([
                        TxnOp::delete(manifest_key, None),
                        TxnOp::delete(placement_key, None),
                    ]),
            )
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        if response.succeeded() {
            Ok(())
        } else {
            Err(StateError::Conflict)
        }
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
            .client
            .as_ref()
            .clone()
            .txn(transaction)
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        if response.succeeded() {
            Ok(())
        } else {
            Err(StateError::AlreadyExists)
        }
    }
}

#[derive(Clone, Default)]
pub struct MemoryStateStore {
    values: Arc<RwLock<BTreeMap<String, Vec<u8>>>>,
}

#[async_trait]
impl ClusterStateStore for MemoryStateStore {
    async fn put<T: Serialize + Send + Sync>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), StateError> {
        let record = serde_json::to_vec(&VersionedRecord {
            version: RECORD_VERSION,
            value,
        })?;
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
    use std::collections::BTreeMap;
    use ulid::Ulid;

    fn memory_store() -> StateStore {
        StateStore::Memory(MemoryStateStore::default())
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

    #[tokio::test]
    async fn paired_create_is_atomic_idempotent_and_conflict_safe() {
        let store = memory_store();
        let vmid = Ulid::generate();
        let (manifest, placement) = create_pair(&store, vmid).await;
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
        drop(store);
    }

    #[tokio::test]
    async fn paired_snapshot_rejects_orphans_and_returns_consistent_records() {
        let store = memory_store();
        let vmid = Ulid::generate();
        store
            .put(
                &key(VM_MANIFESTS_PREFIX, &vmid),
                &serde_json::json!({"id": vmid}),
            )
            .await
            .unwrap();
        assert!(matches!(
            store.list_vm_state::<serde_json::Value>().await,
            Err(StateError::Incomplete)
        ));
        let placement = placement(vmid);
        store
            .put(&key(PLACEMENT_PREFIX, &vmid), &placement)
            .await
            .unwrap();
        let snapshot = store.list_vm_state::<serde_json::Value>().await.unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].placement, placement);
        drop(store);
    }

    #[tokio::test]
    async fn stop_marker_is_durable_and_preserves_manager_owner_and_generation() {
        let store = memory_store();
        let vmid = Ulid::generate();
        let (manifest, original) = create_pair(&store, vmid).await;
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
        drop(store);
    }

    #[tokio::test]
    async fn failed_or_ambiguous_teardown_leaves_both_records_intact() {
        let store = memory_store();
        let vmid = Ulid::generate();
        let (manifest, _) = create_pair(&store, vmid).await;
        let marker = store.begin_vm_stop(vmid, StopIntent::Delete).await.unwrap();
        // No completion call represents an absent or uncertain owner acknowledgement.
        let state = store
            .get_vm_state::<serde_json::Value>(vmid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.manifest, manifest);
        assert_eq!(state.placement, marker);
        drop(store);
    }

    #[tokio::test]
    async fn completion_compare_rejects_stale_generation_and_retries_exactly() {
        let store = memory_store();
        let vmid = Ulid::generate();
        create_pair(&store, vmid).await;
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
        drop(store);
    }

    #[tokio::test]
    async fn completion_rejects_incomplete_pairs_without_deleting_the_remaining_record() {
        let store = memory_store();
        let vmid = Ulid::generate();
        create_pair(&store, vmid).await;
        let expected = store.begin_vm_stop(vmid, StopIntent::Delete).await.unwrap();
        let manifest_key = key(VM_MANIFESTS_PREFIX, &vmid);
        let placement_key = key(PLACEMENT_PREFIX, &vmid);
        store.delete(&manifest_key).await.unwrap();

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
