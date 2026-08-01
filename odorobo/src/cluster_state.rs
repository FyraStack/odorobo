//! Durable desired cluster state.
//!
//! Keys are versioned and namespaced as `/odorobo/v1/{kind}/{id}`. Values are
//! JSON records with a `version` field so readers can reject unknown versions
//! rather than silently misinterpreting state.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use etcd_client::{Certificate, Client, ConnectOptions, TlsOptions};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::sync::RwLock;

pub const KEY_PREFIX: &str = "/odorobo/v1";
pub const VM_MANIFESTS_PREFIX: &str = "/odorobo/v1/vm-manifests";
pub const PLACEMENT_PREFIX: &str = "/odorobo/v1/placement";
pub const NODE_STATE_PREFIX: &str = "/odorobo/v1/node-state";
pub const OPERATIONS_PREFIX: &str = "/odorobo/v1/operations";
pub const RECORD_VERSION: u16 = 1;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("state store operation failed: {0}")]
    Backend(String),
    #[error("state record has unsupported version {0}")]
    UnsupportedVersion(u16),
    #[error("state record is missing")]
    Missing,
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
    async fn delete(&self, key: &str) -> Result<(), StateError>;
    async fn health(&self) -> StoreHealth;
}

pub fn key<T: AsRef<str>>(prefix: &str, id: T) -> String {
    format!("{prefix}/{}", id.as_ref())
}

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

#[derive(Default)]
pub struct MemoryStateStore {
    values: RwLock<BTreeMap<String, Vec<u8>>>,
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
    use super::{ClusterStateStore, MemoryStateStore, VM_MANIFESTS_PREFIX, key};

    #[tokio::test]
    async fn round_trips_versioned_records_without_destructive_reads() {
        let store = MemoryStateStore::default();
        let key = key(VM_MANIFESTS_PREFIX, "vm-1");
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
        assert!(
            store
                .get::<serde_json::Value>(&key)
                .await
                .unwrap()
                .is_some()
        );
    }
}
