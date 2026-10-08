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
        let mut parts = line.split_whitespace();
        let _id = parts.next();
        let Some(pool) = parts.next() else {
            continue;
        };
        let Some(field) = parts.next() else {
            continue;
        };
        let Some(next) = parts.next() else {
            continue;
        };
        let Some(fifth) = parts.next() else {
            continue;
        };

        // id  pool               namespace  image    snap  device
        // 0   pool               foo        testimg    -   /dev/rbd0
        if let Some(device) = parts.next() {
            let identity = format!("{pool}/{field}/{next}");
            mappings.push((
                if fifth == "-" {
                    identity
                } else {
                    format!("{identity}@{fifth}")
                },
                device.to_owned(),
            ));
        } else if fifth.starts_with("/dev/") {
            // If namespace is empty, it may be omitted from the output.
            let identity = format!("{pool}/{field}");
            mappings.push((
                if next == "-" {
                    identity
                } else {
                    format!("{identity}@{next}")
                },
                fifth.to_owned(),
            ));
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

    /// Acquire a new mapping. Never adopt another consumer's mapping.
    #[tracing::instrument(skip(self))]
    pub async fn map(&self) -> Result<PathBuf> {
        let rbd_path = self.rbd_path();
        info!(?rbd_path, "Mapping RBD image to device");
        let output = Command::new("rbd")
            .args(rbd_extra_args())
            .arg("device")
            .arg("map")
            .arg("--options")
            .arg("noudev")
            .arg(&rbd_path)
            .output()
            .await
            .map_err(|e| eyre!("Failed to execute rbd command: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("rbd map failed: {stderr}"));
        }
        let device = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !device.starts_with("/dev/rbd") {
            return Err(eyre!("invalid RBD mapping device {device:?}"));
        }
        Ok(PathBuf::from(device))
    }

    /// Unmaps the RBD image from the kernel block device.
    #[tracing::instrument(skip(self))]
    pub async fn unmap(&self) -> Result<()> {
        let rbd_path = self.rbd_path();
        if !rbd_map_list()
            .await?
            .iter()
            .any(|(path, _)| path == &rbd_path)
        {
            return Ok(());
        }
        info!(?rbd_path, "Unmapping RBD image");
        let output = Command::new("rbd")
            .args(rbd_extra_args())
            .arg("device")
            .arg("unmap")
            .arg("--options")
            .arg("noudev")
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
            .to_owned();
        let path = uri.path();
        if path.is_empty() || path == "/" {
            return Err(eyre!("RBD URI must have a path (image name)"));
        }
        let image = path.trim_start_matches('/').to_owned();
        Ok(Self { pool, image })
    }
}

fn owned_mapping_device<'a>(
    identity: &str,
    expected: &std::path::Path,
    mappings: &'a [(String, String)],
) -> Result<Option<&'a str>> {
    let Some((_, device)) = mappings.iter().find(|(image, _)| image == identity) else {
        return Ok(None);
    };
    if std::path::Path::new(device) != expected {
        return Err(eyre!("RBD mapping identity changed; refusing release"));
    }
    Ok(Some(device))
}

#[derive(Default)]
pub struct RbdStorage {
    owned: tokio::sync::Mutex<std::collections::HashMap<String, Option<PathBuf>>>,
}

#[async_trait]
impl StorageDriver for RbdStorage {
    fn scheme(&self) -> &'static str {
        "rbd"
    }

    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        let _operation = crate::ch_driver::containers::PERSISTENT_OPERATIONS
            .lock()
            .await;
        let image = RbdImage::try_from(uri)?;
        let mappings = rbd_map_list().await?;
        if crate::ch_driver::containers::RBD_RESERVATIONS
            .lock()
            .unwrap()
            .contains(&image.rbd_path())
            || mappings
                .iter()
                .any(|(identity, _)| identity == &image.rbd_path())
        {
            return Err(eyre!("RBD image already mapped; refusing adoption"));
        }
        crate::ch_driver::containers::RBD_RESERVATIONS
            .lock()
            .unwrap()
            .insert(image.rbd_path());
        self.owned
            .lock()
            .await
            .insert(uri.as_str().to_owned(), None);
        let device = image.map().await?;
        self.owned
            .lock()
            .await
            .insert(uri.as_str().to_owned(), Some(device.clone()));
        // map() uses `--options noudev`: with udev enabled, the rbd CLI waits
        // for a udev event in its own network namespace, but rbd devices are
        // created in the host's namespace, so that wait never completes in a
        // container. The kernel uevent still reaches the host's udevd, so the
        // stable path appears shortly after the map. Wait up to 10s (100 x
        // 100ms steps) for it, and fall back to the kernel device name (e.g.
        // /dev/rbd0) when the host has no ceph udev rule.
        let udev_path = image.device_path();
        for _ in 0..100 {
            if udev_path.exists() {
                return Ok(udev_path);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        info!(
            ?device,
            "udev path not available, using owned kernel device name"
        );
        Ok(device)
    }

    async fn release(&self, uri: &Url) -> Result<()> {
        let _operation = crate::ch_driver::containers::PERSISTENT_OPERATIONS
            .lock()
            .await;
        let mut owned = self.owned.lock().await;
        let Some(expected) = owned.get(uri.as_str()) else {
            return Ok(());
        };
        let image = RbdImage::try_from(uri)?;
        let mappings = rbd_map_list().await?;
        let current_device = if let Some(device) = expected {
            owned_mapping_device(&image.rbd_path(), device, &mappings)?
        } else {
            mappings
                .iter()
                .find(|(identity, _)| identity == &image.rbd_path())
                .map(|(_, device)| device.as_str())
        };
        let Some(current_device) = current_device else {
            // Original image detached; the saved device may now be another
            // consumer's mapping. Never unmap by stale pathname existence.
            owned.remove(uri.as_str());
            crate::ch_driver::containers::RBD_RESERVATIONS
                .lock()
                .unwrap()
                .remove(&image.rbd_path());
            return Ok(());
        };
        let output = Command::new("rbd")
            .args(rbd_extra_args())
            .args(["device", "unmap", "--options", "noudev"])
            .arg(current_device)
            .output()
            .await?;
        if !output.status.success() {
            return Err(eyre!(
                "unmap owned RBD device failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        owned.remove(uri.as_str());
        crate::ch_driver::containers::RBD_RESERVATIONS
            .lock()
            .unwrap()
            .remove(&image.rbd_path());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_device_reuse_is_not_attachment_ownership() {
        let mappings = vec![("pool/other-image".into(), "/dev/rbd0".into())];
        assert!(
            owned_mapping_device(
                "pool/original",
                std::path::Path::new("/dev/rbd0"),
                &mappings
            )
            .unwrap()
            .is_none()
        );
        let mappings = vec![("pool/original".into(), "/dev/rbd1".into())];
        assert!(
            owned_mapping_device(
                "pool/original",
                std::path::Path::new("/dev/rbd0"),
                &mappings
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn unowned_failed_attempt_release_never_queries_or_unmaps_other_resources() {
        // No RBD executable/cluster is needed: absence of an ownership token
        // must return before any lookup, even if another mapping exists.
        RbdStorage::default()
            .release(&Url::parse("rbd://pool/existing-other-owner").unwrap())
            .await
            .unwrap();
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
        let mappings =
            rbd_lines_list("id pool namespace image snap device\n0 pool foo testimg - /dev/rbd0")
                .unwrap();
        assert_eq!(mappings[0].0, "pool/foo/testimg");
        let snapshots = rbd_lines_list("id pool namespace image snap device\n0 pool image snap /dev/rbd1\n1 pool foo image snap /dev/rbd2").unwrap();
        assert_eq!(snapshots[0].0, "pool/image@snap");
        assert_eq!(snapshots[1].0, "pool/foo/image@snap");
        assert_eq!(
            owned_mapping_device(
                "pool/foo/testimg",
                std::path::Path::new("/dev/rbd0"),
                &mappings
            )
            .unwrap(),
            Some("/dev/rbd0")
        );
    }
}
