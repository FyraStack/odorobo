use super::{StorageAcquisition, StorageDriver, iscsi::command};
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
    let output = command::run(
        Command::new("rbd")
            .args(rbd_extra_args())
            .arg("device")
            .arg("list"),
        "rbd device list",
    )
    .await
    .map_err(command::CommandFailure::into_report)?;
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
            let image_path = if field == "-" {
                format!("{pool}/{next}")
            } else {
                format!("{pool}/{field}/{next}")
            };
            let image_path = if fifth == "-" {
                image_path
            } else {
                format!("{image_path}@{fifth}")
            };
            mappings.push((image_path, device.to_owned()));
        } else if fifth.starts_with("/dev/") {
            // If namespace is empty, it may be omitted from the output.
            let image_path = if next == "-" {
                format!("{pool}/{field}")
            } else {
                format!("{pool}/{field}@{next}")
            };
            mappings.push((image_path, fifth.to_owned()));
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

    /// Maps the RBD image, borrowing existing mappings rather than owning them.
    #[tracing::instrument(skip(self))]
    pub async fn map(&self) -> Result<StorageAcquisition> {
        let rbd_path = self.rbd_path();
        let mappings = rbd_map_list().await?;

        if mappings.iter().any(|(path, _)| path == &rbd_path) {
            info!(?rbd_path, "RBD image already mapped, reusing");
            return Ok(StorageAcquisition::borrowed());
        }

        info!(?rbd_path, "Mapping RBD image to device");
        let output = match command::run(
            Command::new("rbd")
                .args(rbd_extra_args())
                .arg("device")
                .arg("map")
                .arg("--options")
                .arg("noudev")
                .arg(&rbd_path),
            "rbd device map",
        )
        .await
        {
            Ok(output) => output,
            Err(error) => return error.into_acquisition(),
        };
        if output.status.success() {
            return Ok(StorageAcquisition::owned());
        }
        let error = eyre!(
            "rbd map failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Absence before the command does not attribute a later mapping to us:
        // another process may have mapped it while our command failed.
        let exists = rbd_map_list()
            .await
            .map(|mappings| mappings.iter().any(|(path, _)| path == &rbd_path));
        StorageAcquisition::after_failed_command(error, exists)
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
        let output = command::run(
            Command::new("rbd")
                .args(rbd_extra_args())
                .arg("device")
                .arg("unmap")
                .arg("--options")
                .arg("noudev")
                .arg(&rbd_path),
            "rbd device unmap",
        )
        .await
        .map_err(command::CommandFailure::into_release_report)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if rbd_map_list()
                .await?
                .iter()
                .any(|(path, _)| path == &rbd_path)
            {
                return Err(eyre!("rbd unmap failed: {stderr}"));
            }
        }
        Ok(())
    }
}

impl RbdImage {
    /// Validate the original URI before URL parsing can normalize dot segments or
    /// strip URL whitespace/control characters. The raw lowercase contract is
    /// enforced here; the `rbd` URL parser does not normalize pool-name casing.
    pub(super) fn parse_uri(value: &str) -> Result<Self> {
        let resource = value
            .strip_prefix("rbd://")
            .ok_or_else(|| eyre!("RBD storage URI must use the rbd:// scheme"))?;
        let (pool, image) = resource
            .split_once('/')
            .ok_or_else(|| eyre!("RBD URI must have a pool and one image path segment"))?;
        if pool != pool.to_ascii_lowercase() {
            return Err(eyre!("RBD pool names in the URI must be lowercase"));
        }
        validate_rbd_component(pool, "pool")?;
        if image.contains('/') {
            return Err(eyre!(
                "RBD URI supports only rbd://<pool>/<image>; namespaces and nested image paths are not supported"
            ));
        }
        validate_rbd_component(image, "image")?;

        let parsed =
            Url::parse(value).map_err(|error| eyre!("Invalid RBD storage URI: {error}"))?;
        let parsed_image = Self::try_from(&parsed)?;
        if parsed_image.pool != pool || parsed_image.image != image {
            // Refusing any parser rewrite avoids silently selecting a different
            // pool or image than the original URI named.
            return Err(eyre!(
                "RBD URI components must not change during URL normalization"
            ));
        }
        Ok(parsed_image)
    }
}

impl TryFrom<&Url> for RbdImage {
    type Error = stable_eyre::Report;

    fn try_from(uri: &Url) -> Result<Self, Self::Error> {
        if uri.scheme() != "rbd" {
            return Err(eyre!("RBD storage URI must use the rbd:// scheme"));
        }
        if !uri.username().is_empty()
            || uri.password().is_some()
            || uri.port().is_some()
            || uri.query().is_some()
            || uri.fragment().is_some()
        {
            return Err(eyre!(
                "RBD storage URI must not include credentials, a port, query, or fragment"
            ));
        }

        let pool = uri
            .host_str()
            .ok_or_else(|| eyre!("RBD URI must have a host (pool name)"))?
            .to_owned();
        validate_rbd_component(&pool, "pool")?;

        let image = uri
            .path()
            .strip_prefix('/')
            .ok_or_else(|| eyre!("RBD URI must have exactly one image path segment"))?;
        if image.contains('/') {
            return Err(eyre!(
                "RBD URI supports only rbd://<pool>/<image>; namespaces and nested image paths are not supported"
            ));
        }
        validate_rbd_component(image, "image")?;

        Ok(Self {
            pool,
            image: image.to_owned(),
        })
    }
}

fn validate_rbd_component(value: &str, name: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.starts_with('-')
        || value.contains('%')
        || value.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
        })
    {
        return Err(eyre!(
            "RBD {name} must be a non-empty name using only ASCII letters, digits, '.', '_' or '-'"
        ));
    }
    Ok(())
}

pub struct RbdStorage;

#[async_trait]
impl StorageDriver for RbdStorage {
    fn scheme(&self) -> &'static str {
        "rbd"
    }

    async fn canonical_uri(&self, uri: &Url) -> Result<Url> {
        // Unlike generic URI options, RBD query/fragment data is not meaningful
        // to the backend. Reject it instead of silently dropping it and mapping
        // a different image than the manifest requested.
        RbdImage::try_from(uri)?;
        Ok(uri.clone())
    }

    fn resource_key(&self, uri: &Url) -> Result<String> {
        Ok(format!("rbd://{}", RbdImage::try_from(uri)?.rbd_path()))
    }

    async fn acquire(&self, uri: &Url) -> Result<StorageAcquisition> {
        RbdImage::try_from(uri)?.map().await
    }

    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        let image = RbdImage::try_from(uri)?;
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
        let device = rbd_map_list()
            .await?
            .into_iter()
            .find(|(path, _)| path == &image.rbd_path())
            .map(|(_, device)| device)
            .ok_or_else(|| {
                eyre!(
                    "RBD image {} is mapped but no device name was found",
                    image.rbd_path()
                )
            })?;
        info!(?device, "udev path not available, using kernel device name");
        Ok(PathBuf::from(device))
    }

    async fn release(&self, uri: &Url) -> Result<()> {
        let image = RbdImage::try_from(uri)?;
        image.unmap().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn rejects_unsupported_rbd_uri_components() {
        for input in [
            "file:///pool/image",
            "rbd:///image",
            "rbd://pool/",
            "rbd://pool/image/",
            "rbd://pool/ns/image",
            "rbd://pool/image@snapshot",
            "rbd://pool/image?read_only=true",
            "rbd://pool/image#snapshot",
            "rbd://user@pool/image",
            "rbd://pool:6789/image",
            "rbd://pool/has%20space",
            "rbd://pool/-option",
        ] {
            let uri = Url::parse(input).expect("test URI parses");
            assert!(RbdImage::try_from(&uri).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn mapping_identity_includes_namespace_and_snapshot() {
        let mappings = rbd_lines_list(
            "id pool namespace image snap device\n\
            0 pool - image - /dev/rbd0\n\
            1 pool ns image - /dev/rbd1\n\
            2 pool ns image snap /dev/rbd2\n\
            3 pool image snap /dev/rbd3",
        )
        .unwrap();
        assert_eq!(
            mappings,
            [
                ("pool/image".to_owned(), "/dev/rbd0".to_owned()),
                ("pool/ns/image".to_owned(), "/dev/rbd1".to_owned()),
                ("pool/ns/image@snap".to_owned(), "/dev/rbd2".to_owned()),
                ("pool/image@snap".to_owned(), "/dev/rbd3".to_owned()),
            ]
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
}
