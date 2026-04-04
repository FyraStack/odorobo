use super::StorageBackend;
use async_trait::async_trait;
use stable_eyre::{Result, eyre::eyre};
use std::path::PathBuf;
use tokio::process::Command;
use tracing::{info, trace};
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
#[tracing::instrument]
async fn rbd_map_list() -> Result<Vec<(String, String)>> {
    // returns a list of (rbd_path, device_path) for all currently mapped rbd devices
    let output = Command::new("rbd")
        .args(rbd_extra_args())
        .arg("device")
        .arg("list")
        .output()
        .await
        .map_err(|e| eyre!("Failed to execute rbd command: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("rbd command failed: {stderr}"));
    }

    let output_str = String::from_utf8_lossy(&output.stdout);
    rbd_lines_list(&output_str)
}

#[tracing::instrument]
fn rbd_lines_list(input: &str) -> Result<Vec<(String, String)>> {
    let mut mappings = Vec::new();
    for line in input.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // id  pool               namespace  image    snap  device
        // 0   pool               foo        testimg    -   /dev/rbd0
        if parts.len() == 6 {
            let rbd_path = format!("{}/{}", parts[1], parts[3]);
            let device_path = parts[5].to_string();
            mappings.push((rbd_path, device_path));
        }
        if parts.len() == 5 {
            // if namespace is empty, it might be omitted from the output, so we need to handle that case as well
            // id  pool               image    snap  device
            // 0   pool               testimg    -   /dev/rbd0
            let rbd_path = format!("{}/{}", parts[1], parts[2]);
            let device_path = parts[4].to_string();
            mappings.push((rbd_path, device_path));
        }
    }

    trace!(?mappings, "Parsed RBD device list");
    Ok(mappings)
}

/// Maps the given RBD image to a local block device and returns the device path.
/// If already mapped, returns the existing device path.
#[tracing::instrument]
async fn map_device(rbd_path: &str) -> Result<String> {
    let mappings = rbd_map_list().await?;

    if let Some((_, device_path)) = mappings.into_iter().find(|(path, _)| path == rbd_path) {
        info!(
            ?device_path,
            "RBD image already mapped to device, returning existing mapping"
        );
        Ok(device_path)
    } else {
        info!(?rbd_path, "Mapping RBD image to device");
        let output = Command::new("rbd")
            .args(rbd_extra_args())
            .arg("device")
            .arg("map")
            .arg(rbd_path)
            .output()
            .await
            .map_err(|e| eyre!("Failed to execute rbd command: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("rbd command failed: {stderr}"));
        }

        let output_str = String::from_utf8_lossy(&output.stdout).trim().to_string();

        info!(?output_str, "RBD image mapped to device");

        Ok(output_str)
    }
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
        // format is rbd://pool/image; map and return the udev-stable path at /dev/rbd/<pool>/<image>
        let rbd_path = resolve_rbd_path(uri)?;
        map_device(&rbd_path).await?;
        Ok(PathBuf::from(format!("/dev/rbd/{}", rbd_path)))
    }

    async fn release(&self, uri: &Url) -> Result<()> {
        let rbd_path = resolve_rbd_path(uri)?;
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
        Ok(())
    }
}

#[test]
fn test_resolve_rbd_path() {
    let uri = Url::parse("rbd://my-pool/my-image").unwrap();
    let resolved = resolve_rbd_path(&uri).unwrap();
    assert_eq!(resolved, "my-pool/my-image");
}

#[test]
fn test_rbd_lines_list() {
    let input = "\
id  pool               namespace  image    snap  device
0   kessoku-blockpool             testimg  -     /dev/rbd0";
    let mappings = rbd_lines_list(input).unwrap();
    assert_eq!(mappings.len(), 1);
    assert_eq!(mappings[0].0, "kessoku-blockpool/testimg");
    assert_eq!(mappings[0].1, "/dev/rbd0");
}
