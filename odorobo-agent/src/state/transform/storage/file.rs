//! Simple file URI storage backend for disk images,
//! resolves file:// URIs to local file paths without modification,
//! as if we're simply just stripping the prefix
//! e.g. `file:///path/to/disk.img` -> `/path/to/disk.img`

use super::StorageBackend;
use async_trait::async_trait;
use stable_eyre::{Result, eyre::eyre};
use std::path::PathBuf;
use url::Url;

pub struct FileTarget {
    pub path: PathBuf,
}

impl TryFrom<&Url> for FileTarget {
    type Error = stable_eyre::Report;

    fn try_from(uri: &Url) -> Result<Self, Self::Error> {
        let path = uri
            .to_file_path()
            .map_err(|_| eyre!("Failed to convert file URI to path: '{}'", uri.as_str()))?;
        Ok(FileTarget { path })
    }
}

pub struct FileStorage;

#[async_trait]
impl StorageBackend for FileStorage {
    fn scheme(&self) -> &'static str {
        "file"
    }

    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        Ok(FileTarget::try_from(uri)?.path)
    }

    async fn release(&self, _uri: &Url) -> Result<()> {
        // No resources to release for file storage
        Ok(())
    }
}
