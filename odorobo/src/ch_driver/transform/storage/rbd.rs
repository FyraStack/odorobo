use super::StorageDriver;
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

#[derive(Debug, Clone)]
pub struct RbdImage {
    pub pool: String,
    pub image: String,
}

impl RbdImage {
    pub fn rbd_path(&self) -> String {
        format!("{}/{}", self.pool, self.image)
    }

    /// Returns the udev-stable device path at `/dev/rbd/<pool>/<image>`.
    pub fn device_path(&self) -> PathBuf {
        PathBuf::from(format!("/dev/rbd/{}/{}", self.pool, self.image))
    }

    /// Maps the RBD image to a kernel block device. If already mapped, this is a no-op.
    #[tracing::instrument(skip(self))]
    pub async fn map(&self) -> Result<()> {
        let rbd_path = self.rbd_path();
        let mappings = rbd_map_list().await?;

        if mappings.iter().any(|(path, _)| path == &rbd_path) {
            info!(?rbd_path, "RBD image already mapped, reusing");
            return Ok(());
        }

        info!(?rbd_path, "Mapping RBD image to device");
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
            return Err(eyre!("rbd map failed: {stderr}"));
        }
        Ok(())
    }

    /// Unmaps the RBD image from the kernel block device.
    #[tracing::instrument(skip(self))]
    pub async fn unmap(&self) -> Result<()> {
        let rbd_path = self.rbd_path();
        info!(?rbd_path, "Unmapping RBD image");
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

impl TryFrom<&Url> for RbdImage {
    type Error = stable_eyre::Report;

    fn try_from(uri: &Url) -> Result<Self, Self::Error> {
        let pool = uri
            .host_str()
            .ok_or_else(|| eyre!("RBD URI must have a host (pool name)"))?
            .to_string();
        let path = uri.path();
        if path.is_empty() || path == "/" {
            return Err(eyre!("RBD URI must have a path (image name)"));
        }
        let image = path.trim_start_matches('/').to_string();
        Ok(RbdImage { pool, image })
    }
}

pub struct RbdStorage;

#[async_trait]
impl StorageDriver for RbdStorage {
    fn scheme(&self) -> &'static str {
        "rbd"
    }

    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        let image = RbdImage::try_from(uri)?;
        image.map().await?;
        Ok(image.device_path())
    }

    async fn release(&self, uri: &Url) -> Result<()> {
        let image = RbdImage::try_from(uri)?;
        image.unmap().await
    }
}

#[test]
fn test_rbd_image_from_uri() {
    let uri = Url::parse("rbd://my-pool/my-image").unwrap();
    let image = RbdImage::try_from(&uri).unwrap();
    assert_eq!(image.pool, "my-pool");
    assert_eq!(image.image, "my-image");
    assert_eq!(image.rbd_path(), "my-pool/my-image");
    assert_eq!(
        image.device_path(),
        std::path::PathBuf::from("/dev/rbd/my-pool/my-image")
    );
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
