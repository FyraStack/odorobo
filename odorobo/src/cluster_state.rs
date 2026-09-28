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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacementRecord {
    pub vmid: Ulid,
    pub node: String,
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
    #[error("state serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VersionedRecord<T> {
    version: u16,
    value: T,
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
    client: Arc<RwLock<Client>>,
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
            match Client::connect(endpoints, Some(options.clone())).await {
                Ok(client) => {
                    return Ok(Self {
                        client: Arc::new(RwLock::new(client)),
                    });
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                    let next_attempt = attempt.saturating_add(1);
                    if next_attempt < attempts {
                        let delay_ms = 100_u64.saturating_mul(u64::from(next_attempt));
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                }
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

    /// Atomically creates the desired manifest and its placement. A VM ID may
    /// not be reused because a retry must not replace an existing VM's intent.
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

    /// Atomically removes both halves of a VM's desired state.
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
            .write()
            .await
            .put(key, record, None)
            .await
            .map(|_| ())
            .map_err(|error| StateError::Backend(error.to_string()))
    }

    async fn get<T: DeserializeOwned + Send>(&self, key: &str) -> Result<Option<T>, StateError> {
        let response = self
            .client
            .write()
            .await
            .get(key, None)
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        let Some(value) = response.kvs().first() else {
            return Ok(None);
        };
        let record: VersionedRecord<T> = serde_json::from_slice(value.value())?;
        if record.version != RECORD_VERSION {
            return Err(StateError::UnsupportedVersion(record.version));
        }
        Ok(Some(record.value))
    }

    async fn list<T: DeserializeOwned + Send>(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, T)>, StateError> {
        let response = self
            .client
            .write()
            .await
            .get(prefix, Some(etcd_client::GetOptions::new().with_prefix()))
            .await
            .map_err(|error| StateError::Backend(error.to_string()))?;
        response
            .kvs()
            .iter()
            .map(|value| {
                let record: VersionedRecord<T> = serde_json::from_slice(value.value())?;
                if record.version != RECORD_VERSION {
                    return Err(StateError::UnsupportedVersion(record.version));
                }
                Ok((value.key_str().unwrap_or_default().to_owned(), record.value))
            })
            .collect()
    }

    async fn delete(&self, key: &str) -> Result<(), StateError> {
        self.client
            .write()
            .await
            .delete(key, None)
            .await
            .map(|_| ())
            .map_err(|error| StateError::Backend(error.to_string()))
    }

    async fn health(&self) -> StoreHealth {
        match self.client.write().await.status().await {
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
            .write()
            .await
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
            .write()
            .await
            .txn(Txn::new().and_then([
                TxnOp::delete(manifest_key, None),
                TxnOp::delete(placement_key, None),
            ]))
            .await
            .map(|_| ())
            .map_err(|error| StateError::Backend(error.to_string()))
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
        let record: VersionedRecord<T> = serde_json::from_slice(&value)?;
        if record.version != RECORD_VERSION {
            return Err(StateError::UnsupportedVersion(record.version));
        }
        Ok(Some(record.value))
    }
    async fn list<T: DeserializeOwned + Send>(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, T)>, StateError> {
        self.values
            .read()
            .await
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| {
                let record: VersionedRecord<T> = serde_json::from_slice(value)?;
                if record.version != RECORD_VERSION {
                    return Err(StateError::UnsupportedVersion(record.version));
                }
                Ok((key.clone(), record.value))
            })
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
        ClusterStateStore, MemoryStateStore, PLACEMENT_PREFIX, PlacementRecord, StateError,
        StateStore, VM_MANIFESTS_PREFIX, key,
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
    }

    #[tokio::test]
    async fn creates_and_deletes_paired_vm_state_atomically() {
        let vmid = Ulid::generate();
        let store = Arc::new(StateStore::Memory(MemoryStateStore::default()));
        let placement = PlacementRecord {
            vmid,
            node: "node-a".to_owned(),
        };

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
}
