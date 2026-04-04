use crate::state::transform::ConfigTransform;
use async_trait::async_trait;
use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::Result;
use std::path::PathBuf;
use tracing::warn;
use url::Url;

mod file;
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
pub trait StorageBackend: Send + Sync {
    /// The URI scheme this backend handles, e.g. `"rbd"`, `"file"`. Used for dispatch in `StorageChain`.
    fn scheme(&self) -> &'static str;

    /// Resolves a URI to a local block device or file path for use in a VM disk config.
    async fn resolve(&self, uri: &Url) -> Result<PathBuf>;

    /// Releases resources associated with a previously resolved URI.
    async fn release(&self, uri: &Url) -> Result<()>;
}

/// A chain of storage backends that dispatches disk URI resolution to the backend
/// whose scheme matches the URI scheme.
///
/// Disk paths that are not URIs or whose scheme has no registered backend are left unchanged.
pub struct StorageChain {
    backends: Vec<Box<dyn StorageBackend>>,
}

impl StorageChain {
    pub fn new() -> Self {
        Self { backends: vec![] }
    }

    pub fn add<B: StorageBackend + 'static>(mut self, backend: B) -> Self {
        self.backends.push(Box::new(backend));
        self
    }
}

impl Default for StorageChain {
    fn default() -> Self {
        Self::new().add(file::FileStorage).add(rbd::RbdStorage)
    }
}

impl ConfigTransform for StorageChain {
    fn transform(&self, _vmid: &str, config: &mut VmConfig) -> Result<()> {
        let Some(disks) = config.disks.as_mut() else {
            return Ok(());
        };

        for disk in disks {
            let Some(ref path) = disk.path.clone() else {
                continue;
            };

            let Ok(uri) = Url::parse(path) else {
                continue;
            };

            let Some(backend) = self.backends.iter().find(|b| b.scheme() == uri.scheme()) else {
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
                .append_pair("id", &disk.id.clone().unwrap_or_else(|| "<unknown>".into()))
                .finish()
                .to_string();

            disk.id = Some(new_disk_id);

            let resolved = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(backend.resolve(&uri))
            })?;

            disk.path = Some(resolved.to_string_lossy().into_owned());
        }

        Ok(())
    }
}
