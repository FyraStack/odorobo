//! Simple file URI storage backend for disk images,
//! resolves file:// URIs to local file paths without modification,
//! as if we're simply just stripping the prefix
//! e.g. `file:///path/to/disk.img` -> `/path/to/disk.img`

use super::StorageBackend;
use async_trait::async_trait;
use stable_eyre::{Result, eyre::eyre};
use std::path::PathBuf;
use url::Url;

pub struct FileStorage;
#[async_trait]
impl StorageBackend for FileStorage {
    fn scheme(&self) -> &'static str {
        "file"
    }

    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        let path = uri
            .to_file_path()
            .map_err(|_| eyre!("Failed to convert file URI to path: '{}'", uri.as_str()))?;
        Ok(path)
    }

    async fn release(&self, _uri: &Url) -> Result<()> {
        // No resources to release for file storage
        Ok(())
    }
}
