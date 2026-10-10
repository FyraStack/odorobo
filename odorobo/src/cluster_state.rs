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

/// The agent hostname selected to run a VM manifest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementLifecycle {
    #[default]
    Active,
    Stopping,
    Deleting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VMStopFence {
    pub generation: Ulid,
    pub owner: String,
    pub lifecycle: PlacementLifecycle,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlacementRecord {
    pub vmid: Ulid,
    pub node: String,
    /// Fences delayed create requests from earlier incarnations of this VM ID.
    #[serde(default)]
    pub generation: Ulid,
    /// Old records without a lifecycle field remain active.
    #[serde(default)]
    pub lifecycle: PlacementLifecycle,
}

impl PlacementRecord {
    #[must_use]
    pub fn active(vmid: Ulid, node: String) -> Self {
        Self {
            vmid,
            node,
            generation: Ulid::generate(),
            lifecycle: PlacementLifecycle::Active,
        }
    }

    #[must_use]
    pub fn stop_fence(&self) -> VMStopFence {
        VMStopFence {
            generation: self.generation,
            owner: self.node.clone(),
            lifecycle: self.lifecycle,
        }
    }

    fn matches_fence(&self, fence: &VMStopFence) -> bool {
        if self.generation != fence.generation {
            return false;
        }
        if self.node != fence.owner {
            return false;
        }
        self.lifecycle == fence.lifecycle && self.lifecycle != PlacementLifecycle::Active
    }
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
    #[error("state record already exists")]
    AlreadyExists,
    #[error("state changed concurrently or lifecycle transition is not allowed")]
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

    /// Atomically creates the desired manifest and its placement. A retry while
    /// either key exists cannot replace a VM's current intent or placement.
    pub async fn create_vm_state<T: Serialize + Send + Sync>(
        &self,
        vmid: Ulid,
        manifest: &T,
        placement: &PlacementRecord,
    ) -> Result<(), StateError> {
        let manifest_key = key(VM_MANIFESTS_PREFIX, &vmid);
        let placement_key = key(PLACEMENT_PREFIX, &vmid);
        let manifest = serde_json::to_vec(&VersionedRecord {
            version: RECORD_VERSION,
            value: manifest,
        })?;
        let placement = serde_json::to_vec(&VersionedRecord {
            version: RECORD_VERSION,
            value: placement,
        })?;

        match self {
            Self::Etcd(store) => {
                store
                    .create_vm_state(&manifest_key, manifest, &placement_key, placement)
                    .await
            }
            Self::Memory(store) => {
                let mut values = store.values.write().await;
                if values.contains_key(&manifest_key) || values.contains_key(&placement_key) {
                    drop(values);
                    return Err(StateError::AlreadyExists);
                }
                values.insert(manifest_key, manifest);
                values.insert(placement_key, placement);
                drop(values);
                Ok(())
            }
        }
    }

    /// Atomically assigns a previously unplaced VM. Existing placement is never
    /// overwritten by a manager acting on stale observations.
    pub async fn assign_placement_if_absent(
        &self,
        placement: &PlacementRecord,
    ) -> Result<(), StateError> {
        let placement_key = key(PLACEMENT_PREFIX, &placement.vmid);
        let manifest_key = key(VM_MANIFESTS_PREFIX, &placement.vmid);
        match self {
            Self::Etcd(store) => {
                store
                    .assign_placement_if_absent(&manifest_key, &placement_key, placement)
                    .await
            }
            Self::Memory(store) => {
                let mut values = store.values.write().await;
                if values.contains_key(&placement_key) {
                    return Err(StateError::AlreadyExists);
                }
                if !values.contains_key(&manifest_key) {
                    return Err(StateError::Missing);
                }
                let record = serde_json::to_vec(&VersionedRecord {
                    version: RECORD_VERSION,
                    value: placement,
                })?;
                values.insert(placement_key, record);
                drop(values);
                Ok(())
            }
        }
    }

    /// Marks a VM as stopping before any runtime teardown is dispatched.
    /// Repeated requests preserve the first durable stop intent.
    pub async fn begin_vm_stop(
        &self,
        vmid: Ulid,
        intent: StopIntent,
    ) -> Result<PlacementRecord, StateError> {
        let placement_key = key(PLACEMENT_PREFIX, &vmid);
        match self {
            Self::Etcd(store) => store.begin_vm_stop(&placement_key, intent).await,
            Self::Memory(store) => {
                let mut values = store.values.write().await;
                let encoded = values.get(&placement_key).ok_or(StateError::Missing)?;
                let mut placement: PlacementRecord = decode_record(encoded)?;
                if placement.lifecycle == PlacementLifecycle::Active {
                    placement.lifecycle = intent.lifecycle();
                    let record = serde_json::to_vec(&VersionedRecord {
                        version: RECORD_VERSION,
                        value: &placement,
                    })?;
                    values.insert(placement_key, record);
                }
                drop(values);
                Ok(placement)
            }
        }
    }

    /// Removes desired state only after runtime teardown has been confirmed and
    /// only if the placement still identifies the exact stopped incarnation.
    /// The operation is idempotent once both records have been removed.
    pub async fn complete_vm_stop(&self, expected: &PlacementRecord) -> Result<(), StateError> {
        let manifest_key = key(VM_MANIFESTS_PREFIX, &expected.vmid);
        let placement_key = key(PLACEMENT_PREFIX, &expected.vmid);
        match self {
            Self::Etcd(store) => {
                store
                    .complete_vm_stop(&manifest_key, &placement_key, &expected.stop_fence())
                    .await
            }
            Self::Memory(store) => {
                let mut values = store.values.write().await;
                if let Some(encoded) = values.get(&placement_key) {
                    let placement: PlacementRecord = decode_record(encoded)?;
                    if !placement.matches_fence(&expected.stop_fence()) {
                        return Err(StateError::Conflict);
                    }
                    values.remove(&placement_key);
                    values.remove(&manifest_key);
                } else if values.contains_key(&manifest_key) {
                    return Err(StateError::Conflict);
                }
                drop(values);
                Ok(())
            }
        }
    }

    /// Reads paired VM state as one consistent snapshot.
    pub async fn list_vm_state(
        &self,
    ) -> Result<
        (
            Vec<(String, serde_json::Value)>,
            Vec<(String, PlacementRecord)>,
        ),
        StateError,
    > {
        match self {
            Self::Etcd(store) => store.list_vm_state().await,
            Self::Memory(store) => store.list_vm_state().await,
        }
    }

    /// Atomically removes both halves of a VM's desired state. Kept for callers
    /// that need unconditional cleanup; lifecycle handlers use `complete_vm_stop`.
    pub async fn delete_vm_state(&self, vmid: Ulid) -> Result<(), StateError> {
        let manifest_key = key(VM_MANIFESTS_PREFIX, &vmid);
        let placement_key = key(PLACEMENT_PREFIX, &vmid);
        match self {
            Self::Etcd(store) => store.delete_vm_state(&manifest_key, &placement_key).await,
            Self::Memory(store) => {
                let mut values = store.values.write().await;
                values.remove(&manifest_key);
                values.remove(&placement_key);
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
    async fn list_vm_state(
        &self,
    ) -> Result<
        (
            Vec<(String, serde_json::Value)>,
            Vec<(String, PlacementRecord)>,
        ),
        StateError,
    > {
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
        let manifests_prefix = list_prefix(VM_MANIFESTS_PREFIX);
        let placements_prefix = list_prefix(PLACEMENT_PREFIX);
        let mut manifests = Vec::new();
        let mut placements = Vec::new();
        for kv in response.kvs() {
            let key = kv.key_str().unwrap_or_default().to_owned();
            if key.starts_with(&manifests_prefix) {
                manifests.push((key, decode_record(kv.value())?));
            } else if key.starts_with(&placements_prefix) {
                placements.push((key, decode_record(kv.value())?));
            }
        }
        Ok((manifests, placements))
    }

    async fn assign_placement_if_absent(
        &self,
        manifest_key: &str,
        placement_key: &str,
        placement: &PlacementRecord,
    ) -> Result<(), StateError> {
        let record = serde_json::to_vec(&VersionedRecord {
            version: RECORD_VERSION,
            value: placement,
        })?;
        let response = self
            .client
            .as_ref()
            .clone()
            .txn(
                Txn::new()
                    .when([
                        Compare::version(manifest_key, CompareOp::Greater, 0),
                        Compare::version(placement_key, CompareOp::Equal, 0),
                    ])
                    .and_then([TxnOp::put(placement_key, record, None)]),
            )
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        if response.succeeded() {
            Ok(())
        } else {
            Err(StateError::AlreadyExists)
        }
    }

    async fn begin_vm_stop(
        &self,
        placement_key: &str,
        intent: StopIntent,
    ) -> Result<PlacementRecord, StateError> {
        for _ in 0..5 {
            let response = self
                .client
                .as_ref()
                .clone()
                .get(placement_key, None)
                .await
                .map_err(|error| StateError::Backend(error.to_string()))?;
            let Some(kv) = response.kvs().first() else {
                return Err(StateError::Missing);
            };
            let mut placement: PlacementRecord = decode_record(kv.value())?;
            if placement.lifecycle != PlacementLifecycle::Active {
                return Ok(placement);
            }
            placement.lifecycle = intent.lifecycle();
            let record = serde_json::to_vec(&VersionedRecord {
                version: RECORD_VERSION,
                value: &placement,
            })?;
            let transaction = Txn::new()
                .when([Compare::mod_revision(
                    placement_key,
                    CompareOp::Equal,
                    kv.mod_revision(),
                )])
                .and_then([TxnOp::put(placement_key, record, None)]);
            let response = self
                .client
                .as_ref()
                .clone()
                .txn(transaction)
                .await
                .map_err(|error| StateError::Backend(error.to_string()))?;
            if response.succeeded() {
                return Ok(placement);
            }
        }
        Err(StateError::Conflict)
    }

    async fn complete_vm_stop(
        &self,
        manifest_key: &str,
        placement_key: &str,
        expected: &VMStopFence,
    ) -> Result<(), StateError> {
        let response = self
            .client
            .as_ref()
            .clone()
            .get(placement_key, None)
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        let Some(kv) = response.kvs().first() else {
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
                Err(StateError::Conflict)
            };
        };
        let placement: PlacementRecord = decode_record(kv.value())?;
        if !placement.matches_fence(expected) {
            return Err(StateError::Conflict);
        }
        let response = self
            .client
            .as_ref()
            .clone()
            .txn(
                Txn::new()
                    .when([Compare::mod_revision(
                        placement_key,
                        CompareOp::Equal,
                        kv.mod_revision(),
                    )])
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

    async fn delete_vm_state(
        &self,
        manifest_key: &str,
        placement_key: &str,
    ) -> Result<(), StateError> {
        self.client
            .as_ref()
            .clone()
            .txn(Txn::new().and_then([
                TxnOp::delete(manifest_key, None),
                TxnOp::delete(placement_key, None),
            ]))
            .await
            .map(|_| ())
            .map_err(|error| StateError::Backend(error.to_string()))
    }
}

impl MemoryStateStore {
    async fn list_vm_state(
        &self,
    ) -> Result<
        (
            Vec<(String, serde_json::Value)>,
            Vec<(String, PlacementRecord)>,
        ),
        StateError,
    > {
        let values = self.values.read().await;
        let manifests_prefix = list_prefix(VM_MANIFESTS_PREFIX);
        let placements_prefix = list_prefix(PLACEMENT_PREFIX);
        let mut manifests = Vec::new();
        let mut placements = Vec::new();
        for (key, value) in values.iter() {
            if key.starts_with(&manifests_prefix) {
                manifests.push((key.clone(), decode_record(value)?));
            } else if key.starts_with(&placements_prefix) {
                placements.push((key.clone(), decode_record(value)?));
            }
        }
        drop(values);
        Ok((manifests, placements))
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
        ClusterStateStore, MemoryStateStore, PLACEMENT_PREFIX, PlacementLifecycle, PlacementRecord,
        StateError, StateStore, StopIntent, VM_MANIFESTS_PREFIX, key,
    };
    use std::sync::Arc;
    use ulid::Ulid;

    #[tokio::test]
    async fn round_trips_versioned_records_without_destructive_reads() {
        let store = MemoryStateStore::default();
        let key = key(VM_MANIFESTS_PREFIX, &"vm-1");
        store
            .put(&key, &serde_json::json!({"name": "demo"}))
            .await
            .unwrap();
        assert_eq!(
            store.get::<serde_json::Value>(&key).await.unwrap(),
            Some(serde_json::json!({"name": "demo"}))
        );
        assert!(
            store
                .get::<serde_json::Value>("missing")
                .await
                .unwrap()
                .is_none()
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
                .get::<serde_json::Value>(&key)
                .await
                .unwrap()
                .is_some()
        );
        store.delete(&key).await.unwrap();
        assert!(
            store
                .get::<serde_json::Value>(&key)
                .await
                .unwrap()
                .is_none()
        );

        store.values.write().await.insert(
            key.clone(),
            serde_json::to_vec(&serde_json::json!({"version": 2, "value": {"name": "future"}}))
                .unwrap(),
        );
        assert!(matches!(
            store.get::<serde_json::Value>(&key).await,
            Err(super::StateError::UnsupportedVersion(2))
        ));

        store.values.write().await.insert(
            key.clone(),
            serde_json::to_vec(&serde_json::json!({"version": 2, "value": "future"})).unwrap(),
        );
        assert!(matches!(
            store.get::<PlacementRecord>(&key).await,
            Err(super::StateError::UnsupportedVersion(2))
        ));
    }

    #[tokio::test]
    async fn list_only_matches_children_of_the_requested_prefix() {
        let store = MemoryStateStore::default();
        store
            .put(
                &key(PLACEMENT_PREFIX, &"vm-1"),
                &serde_json::json!({"name": "included"}),
            )
            .await
            .unwrap();
        store
            .put(
                &format!("{PLACEMENT_PREFIX}-backup/vm-2"),
                &serde_json::json!({"name": "excluded"}),
            )
            .await
            .unwrap();

        let records = store
            .list::<serde_json::Value>(PLACEMENT_PREFIX)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].1, serde_json::json!({"name": "included"}));
    }

    #[tokio::test]
    async fn creates_and_deletes_paired_vm_state_atomically() {
        let vmid = Ulid::generate();
        let store = Arc::new(StateStore::Memory(MemoryStateStore::default()));
        let placement = PlacementRecord::active(vmid, "node-a".to_owned());

        store
            .create_vm_state(vmid, &serde_json::json!({"name": "demo"}), &placement)
            .await
            .unwrap();
        assert!(matches!(
            store
                .create_vm_state(vmid, &serde_json::json!({"name": "other"}), &placement)
                .await,
            Err(StateError::AlreadyExists)
        ));

        store.delete_vm_state(vmid).await.unwrap();
        assert!(
            store
                .get::<serde_json::Value>(&key(VM_MANIFESTS_PREFIX, &vmid))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get::<PlacementRecord>(&key(PLACEMENT_PREFIX, &vmid))
                .await
                .unwrap()
                .is_none()
        );
        drop(store);
    }

    #[tokio::test]
    async fn durable_stop_intent_precedes_teardown_and_survives_failed_shutdown() {
        let vmid = Ulid::generate();
        let memory = MemoryStateStore::default();
        let store = StateStore::Memory(memory.clone());
        let manifest_key = key(VM_MANIFESTS_PREFIX, &vmid);
        store
            .create_vm_state(
                vmid,
                &serde_json::json!({"name": "demo"}),
                &PlacementRecord::active(vmid, "node-a".to_owned()),
            )
            .await
            .unwrap();

        let stopping = store
            .begin_vm_stop(vmid, StopIntent::Shutdown)
            .await
            .unwrap();
        assert_eq!(stopping.lifecycle, PlacementLifecycle::Stopping);
        assert!(
            store
                .get::<serde_json::Value>(&manifest_key)
                .await
                .unwrap()
                .is_some()
        );
        drop(store);

        // A restart sees the same stop marker; it must not treat the manifest
        // as active desired state if teardown had failed before confirmation.
        let restarted = StateStore::Memory(memory);
        let recovered = restarted
            .get::<PlacementRecord>(&key(PLACEMENT_PREFIX, &vmid))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.lifecycle, PlacementLifecycle::Stopping);
        assert!(
            restarted
                .get::<serde_json::Value>(&manifest_key)
                .await
                .unwrap()
                .is_some()
        );
        // A failed runtime stop does not call complete_vm_stop; the durable
        // marker remains available for an owner agent to recover it.
        restarted.complete_vm_stop(&recovered).await.unwrap();
        assert!(
            restarted
                .get::<serde_json::Value>(&manifest_key)
                .await
                .unwrap()
                .is_none()
        );
        drop(restarted);
    }

    #[tokio::test]
    async fn delayed_stop_completion_cannot_delete_a_recreated_vm_incarnation() {
        let vmid = Ulid::generate();
        let store = StateStore::Memory(MemoryStateStore::default());
        let manifest = serde_json::json!({"name": "same-vm-id"});
        let generation_one = PlacementRecord::active(vmid, "node-a".to_owned());
        store
            .create_vm_state(vmid, &manifest, &generation_one)
            .await
            .unwrap();
        let stopped_generation_one = store.begin_vm_stop(vmid, StopIntent::Delete).await.unwrap();
        store
            .complete_vm_stop(&stopped_generation_one)
            .await
            .unwrap();

        let generation_two = PlacementRecord::active(vmid, "node-a".to_owned());
        assert_ne!(generation_one.generation, generation_two.generation);
        store
            .create_vm_state(vmid, &manifest, &generation_two)
            .await
            .unwrap();
        let stopped_generation_two = store
            .begin_vm_stop(vmid, StopIntent::Shutdown)
            .await
            .unwrap();

        // The delayed acknowledgement from G1 arrives while G2 is itself in a
        // durable stop transition but has not yet completed runtime teardown.
        assert!(matches!(
            store.complete_vm_stop(&stopped_generation_one).await,
            Err(StateError::Conflict)
        ));
        let current = store
            .get::<PlacementRecord>(&key(PLACEMENT_PREFIX, &vmid))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.generation, generation_two.generation);
        assert_eq!(current.lifecycle, PlacementLifecycle::Stopping);
        assert_eq!(current.generation, stopped_generation_two.generation);
        assert!(
            store
                .get::<serde_json::Value>(&key(VM_MANIFESTS_PREFIX, &vmid))
                .await
                .unwrap()
                .is_some()
        );
        drop(store);
    }

    #[tokio::test]
    async fn placement_assignment_is_create_only_and_stop_intent_is_not_reverted() {
        let vmid = Ulid::generate();
        let store = StateStore::Memory(MemoryStateStore::default());
        store
            .put(
                &key(VM_MANIFESTS_PREFIX, &vmid),
                &serde_json::json!({"name": "demo"}),
            )
            .await
            .unwrap();
        let original = PlacementRecord::active(vmid, "node-a".to_owned());
        store.assign_placement_if_absent(&original).await.unwrap();
        let other = PlacementRecord::active(vmid, "node-b".to_owned());
        assert!(matches!(
            store.assign_placement_if_absent(&other).await,
            Err(StateError::AlreadyExists)
        ));
        assert_eq!(
            store
                .begin_vm_stop(vmid, StopIntent::Delete)
                .await
                .unwrap()
                .node,
            "node-a"
        );
        let repeated = store
            .begin_vm_stop(vmid, StopIntent::Shutdown)
            .await
            .unwrap();
        assert_eq!(repeated.lifecycle, PlacementLifecycle::Deleting);
        assert_eq!(repeated.generation, original.generation);
        drop(store);
    }
}
