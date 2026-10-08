use crate::ch_driver::transform::ConfigTransform;
use async_trait::async_trait;
use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::Result;
use std::path::PathBuf;
use tracing::warn;
use url::Url;

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

    /// Resolves a URI to a local block device or file path for use in a VM disk config.
    async fn resolve(&self, uri: &Url) -> Result<PathBuf>;

    /// Releases resources associated with an attempted URI resolution.
    /// Must succeed as a no-op if the failed attempt left no attachment.
    async fn release(&self, uri: &Url) -> Result<()>;
}

/// A chain of storage backends that dispatches disk URI resolution to the backend
/// whose scheme matches the URI scheme.
///
/// Disk paths that are not URIs or whose scheme has no registered backend are left unchanged.
pub struct StorageDriverTransformer {
    backends: Vec<Box<dyn StorageDriver>>,
    /// Acquisition attempts are separate from untouched desired disk URIs.
    attempts: std::sync::Mutex<std::collections::HashMap<String, Vec<Url>>>,
}

impl StorageDriverTransformer {
    pub fn new() -> Self {
        Self {
            backends: vec![],
            attempts: Default::default(),
        }
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

    /// Releases only recorded acquisition attempts, in reverse order. Failed
    /// releases remain recorded for retry; untouched desired disks are excluded.
    pub async fn release_config(&self, vmid: &str) -> Result<()> {
        loop {
            let uri = self
                .attempts
                .lock()
                .unwrap()
                .get(vmid)
                .and_then(|uris| uris.last())
                .cloned();
            let Some(uri) = uri else {
                return Ok(());
            };
            self.find_backend(&uri)
                .expect("recorded backend")
                .release(&uri)
                .await?;
            self.attempts.lock().unwrap().get_mut(vmid).unwrap().pop();
        }
    }
}

impl Default for StorageDriverTransformer {
    fn default() -> Self {
        Self::new()
            .with_backend(file::FileStorage)
            .with_backend(rbd::RbdStorage::default())
            .with_backend(iscsi::ISCSIStorage::default())
    }
}

impl ConfigTransform for StorageDriverTransformer {
    fn teardown(&self, vmid: &str, _config: &mut VmConfig) -> Result<()> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.release_config(vmid))
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

            let new_disk_id = uri
                .clone()
                .query_pairs_mut()
                .append_pair("id", disk.id.as_deref().unwrap_or("<unknown>"))
                .finish()
                .to_string();

            disk.id = Some(new_disk_id);

            // Record before resolve: a failed attempt may have acquired some
            // resources. Backends must reconcile absence as successful cleanup.
            self.attempts
                .lock()
                .unwrap()
                .entry(vmid.to_owned())
                .or_default()
                .push(uri.clone());
            let resolved = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(backend.resolve(&uri))
            })?;

            disk.path = Some(resolved.to_string_lossy().into_owned());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct RecordingStorage(Arc<Mutex<Vec<String>>>);
    #[async_trait]
    impl StorageDriver for RecordingStorage {
        fn scheme(&self) -> &'static str {
            "recording"
        }
        async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
            self.0
                .lock()
                .unwrap()
                .push(format!("resolve:{}", uri.host_str().unwrap()));
            if uri.host_str() == Some("failed") {
                return Err(stable_eyre::eyre::eyre!("no resource acquired"));
            }
            Ok("/dev/recording".into())
        }
        async fn release(&self, uri: &Url) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("release:{}", uri.host_str().unwrap()));
            Ok(()) // already absent is a no-op
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn partial_resolution_never_releases_untouched_desired_disks() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let transformer =
            StorageDriverTransformer::new().with_backend(RecordingStorage(calls.clone()));
        let mut config = VmConfig {
            disks: Some(
                ["acquired", "failed", "untouched"]
                    .iter()
                    .map(|name| cloud_hypervisor_client::models::DiskConfig {
                        path: Some(format!("recording://{name}/disk")),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        };
        assert!(transformer.transform("fixture", &mut config).is_err());
        transformer.teardown("fixture", &mut config).unwrap();
        transformer.teardown("fixture", &mut config).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "resolve:acquired",
                "resolve:failed",
                "release:failed",
                "release:acquired"
            ]
        );
    }
}
