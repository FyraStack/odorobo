use super::StorageBackend;
use async_trait::async_trait;
use stable_eyre::{Result, eyre::eyre};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use url::Url;

const CEPH_ID_ENV: &str = "CEPH_ID";
const CEPH_KEYFILE_ENV: &str = "CEPH_KEYFILE";
const CEPH_CLUSTER_ENV: &str = "CEPH_CLUSTER";
const CEPH_CONFIG_ENV: &str = "CEPH_CONFIG";

pub struct RbdStorage;

fn rbd_extra_args() -> Vec<String> {
    let mut args = Vec::new();
    if let Ok(ceph_config) = std::env::var(CEPH_CONFIG_ENV) {
        args.push(format!("--conf={ceph_config}"));
    }
    if let Ok(ceph_id) = std::env::var(CEPH_ID_ENV) {
        args.push(format!("--id={ceph_id}"));
    }
    if let Ok(ceph_key) = std::env::var(CEPH_KEYFILE_ENV) {
        args.push(format!("--keyfile={ceph_key}"));
    }
    if let Ok(ceph_cluster) = std::env::var(CEPH_CLUSTER_ENV) {
        args.push(format!("--cluster={ceph_cluster}"));
    }
    args
}

fn resolve_rbd_path(uri: &Url) -> Result<String> {
    let host = uri
        .host_str()
        .ok_or_else(|| eyre!("RBD URI must have a host (pool name)"))?;
    let path = uri.path();
    if path.is_empty() || path == "/" {
        return Err(eyre!("RBD URI must have a path (image name)"));
    }
    Ok(format!("{}{}", host, path))
}

#[async_trait]
impl StorageBackend for RbdStorage {
    fn scheme(&self) -> &'static str {
        "rbd"
    }

    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        // format is rbd://pool/image, we need to map this to a local block device using `rbd device map`

        let rbd_path = resolve_rbd_path(uri)?;

        // Ensure required environment variables are set
        let output = Command::new("rbd")
            .args(rbd_extra_args())
            .arg("device")
            .arg("map")
            .arg(&rbd_path)
            .output()
            .await
            .map_err(|e| eyre!("Failed to execute rbd command: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("rbd command failed: {stderr}"));
        }

        // now we want to symlink this to a stable path under /run/odorobo/disks/rbd/{pool}/{image} so that we can use that path in the VM config and it won't change across reboots or remaps
        let device_path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());

        let stable_path_str = format!("/run/odorobo/disks/rbd/{}", rbd_path);
        let stable_path = Path::new(&stable_path_str);

        tokio::fs::create_dir_all(stable_path.parent().unwrap()).await?;
        if std::path::Path::new(&stable_path).exists() {
            tokio::fs::remove_file(&stable_path).await?;
        }
        tokio::fs::symlink(&device_path, &stable_path).await?;

        Ok(PathBuf::from(stable_path))
    }

    async fn release(&self, uri: &Url) -> Result<()> {
        let rbd_path = resolve_rbd_path(uri)?;
        let stable_path_str = format!("/run/odorobo/disks/rbd/{}", rbd_path);
        let output = Command::new("rbd")
            .args(rbd_extra_args())
            .arg("device")
            .arg("unmap")
            .arg(&rbd_path)
            .output()
            .await
            .map_err(|e| eyre!("Failed to execute rbd command: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("rbd unmap failed: {stderr}"));
        }

        tokio::fs::remove_file(&stable_path_str).await?;
        Ok(())
    }
}

#[test]
fn test_resolve_rbd_path() {
    let uri = Url::parse("rbd://my-pool/my-image").unwrap();
    let resolved = resolve_rbd_path(&uri).unwrap();
    assert_eq!(resolved, "my-pool/my-image");
}
