//! iSCSI initiator transformer for storage backend
//! resolves iscsi:// URIs by logging into the iSCSI target and returning the local
//! block device path for the specified target and LUN, e.g. /dev/disk/by-path/ip-*
use super::StorageBackend;
use async_trait::async_trait;
use serde::Deserialize;
use stable_eyre::{Result, eyre::eyre};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{info, trace};
use url::Url;

/// Struct representation of an iSCSI target,
/// parsed from the URI, e.g.
/// `iscsi://target.example.com:3260/iqn.2024-01.com.example:target1/0` would be parsed into
#[derive(Debug, Clone, Deserialize)]
pub struct ISCSITarget {
    pub host: String,
    pub iqn: String,
    pub lun: u32,
}

impl ISCSITarget {
    pub fn to_device_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/dev/disk/by-path/ip-{}-iscsi-{}-lun-{}",
            self.host, self.iqn, self.lun
        ))
    }

    #[tracing::instrument(skip(self))]
    pub async fn attach(&self) -> Result<PathBuf> {
        // do iscsiadm login to the target, then find the corresponding device path in /dev/disk/by-path
        info!(?self, "Attaching iSCSI target");

        Command::new("iscsiadm")
            .args(["-m", "node", "-T", &self.iqn, "-p", &self.host, "--login"])
            .output()
            .await
            .map_err(|e| eyre!("Failed to execute iscsiadm command: {e}"))?;
        Ok(self.to_device_path())
    }

    #[tracing::instrument(skip(self))]
    pub async fn detach(&self) -> Result<()> {
        info!(?self, "Detaching iSCSI target");
        Command::new("iscsiadm")
            .args(["-m", "node", "-T", &self.iqn, "-p", &self.host, "--logout"])
            .output()
            .await
            .map_err(|e| eyre!("Failed to execute iscsiadm command: {e}"))?;
        Ok(())
    }
}

impl Into<PathBuf> for ISCSITarget {
    fn into(self) -> PathBuf {
        self.to_device_path()
    }
}

impl From<&Url> for ISCSITarget {
    fn from(uri: &Url) -> Self {
        let host_ip = uri.host_str().unwrap_or_default().to_string();
        let port = uri.port().unwrap_or(3260);
        let host = format!("{}:{}", host_ip, port);
        let path_segments: Vec<&str> = uri.path_segments().map(|c| c.collect()).unwrap_or_default();
        let iqn = path_segments.get(0).unwrap_or(&"").to_string();
        let lun_str = path_segments.get(1).unwrap_or(&"");
        let lun = lun_str
            .strip_prefix("lun")
            .unwrap_or(lun_str)
            .parse::<u32>()
            .unwrap_or(0);
        ISCSITarget { host, iqn, lun }
    }
}

fn list_iscsi_devices() -> Result<Vec<PathBuf>> {
    // simply just list /dev/disk/by-path/ip-* for now

    let mut devices = Vec::new();
    let by_path = Path::new("/dev/disk/by-path");
    if by_path.exists() {
        for entry in by_path.read_dir()? {
            let entry = entry?;
            let file_name = entry.file_name();
            let file_name_str = file_name.to_string_lossy();
            if file_name_str.starts_with("ip-") {
                let device_path = entry.path();
                if device_path.exists() {
                    devices.push(device_path);
                }
            }
        }
    }
    Ok(devices)
}

// /dev/disk/by-path/ip-127.0.0.1:3260-iscsi-iqn.2026-03.com.fyrastack:test-lun-0

impl TryFrom<PathBuf> for ISCSITarget {
    type Error = stable_eyre::Report;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        // parse the filename to extract the host, iqn, and lun

        // strip the "ip-" prefix and split by "-iscsi-"
        let stripped = file_name
            .strip_prefix("ip-")
            .ok_or_else(|| eyre!("Device path does not start with 'ip-': {}", path.display()))?;

        let (host_part, rest) = stripped.split_once("-iscsi-").ok_or_else(|| {
            eyre!(
                "Failed to parse iSCSI target from device path: {}",
                path.display()
            )
        })?;

        let (iqn_part, lun_part) = rest.rsplit_once("-lun-").unwrap_or((rest, "0"));
        let host = host_part.to_string();
        let iqn = iqn_part.to_string();
        let lun = lun_part.parse::<u32>().unwrap_or(0);
        Ok(ISCSITarget { host, iqn, lun })
    }
}

pub struct ISCSIStorage;

#[async_trait]
impl StorageBackend for ISCSIStorage {
    fn scheme(&self) -> &'static str {
        "iscsi"
    }
    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        let target = ISCSITarget::from(uri);
        target.attach().await
    }

    async fn release(&self, uri: &Url) -> Result<()> {
        let target = ISCSITarget::from(uri);
        target.detach().await
    }
}

#[test]
fn test_iscsi_target_parsing_from_path() {
    let path = PathBuf::from(
        "/dev/disk/by-path/ip-127.0.0.1:3260-iscsi-iqn.2026-03.com.fyrastack:test-lun-0",
    );
    let target = ISCSITarget::try_from(path).unwrap();
    println!("{:?}", target);

    assert_eq!(target.host, "127.0.0.1:3260");
    assert_eq!(target.iqn, "iqn.2026-03.com.fyrastack:test");
    assert_eq!(target.lun, 0);
}

#[test]
fn test_iscsi_target_parsing() {
    let uri =
        Url::parse("iscsi://target.example.com:3260/iqn.2024-01.com.example:target1/lun0").unwrap();
    let target = ISCSITarget::from(&uri);
    println!("{:?}", target);
    assert_eq!(target.host, "target.example.com:3260");
    assert_eq!(target.iqn, "iqn.2024-01.com.example:target1");
    assert_eq!(target.lun, 0);
    let uri2 = Url::parse("iscsi://target.example.com/iqn.2024-01.com.example:target1/0").unwrap();
    let target2 = ISCSITarget::from(&uri2);
    println!("{:?}", target2);
    assert_eq!(target2.host, "target.example.com:3260");
    assert_eq!(target2.iqn, "iqn.2024-01.com.example:target1");
    assert_eq!(target2.lun, 0);
}

#[test]
fn test_iscsi_lossless_conversion() {
    let uri =
        Url::parse("iscsi://target.example.com:3260/iqn.2024-01.com.example:target1/0").unwrap();
    let target = ISCSITarget::from(&uri);
    let reconstructed_uri = format!("iscsi://{}/{}/{}", target.host, target.iqn, target.lun);
    assert_eq!(uri.as_str(), reconstructed_uri);
}
#[test]
fn test_iscsi_uri_to_device_path() {
    let uri =
        Url::parse("iscsi://target.example.com:3260/iqn.2026-03.com.fyrastack:test/0").unwrap();
    let actual_device_path =
        "/dev/disk/by-path/ip-target.example.com:3260-iscsi-iqn.2026-03.com.fyrastack:test-lun-0";
    let target = ISCSITarget::from(&uri);
    let device_path = target.to_device_path().display().to_string();
    println!("Device path: {}", device_path);
    assert_eq!(device_path, actual_device_path);
}

#[test]
fn test_iscsi_iqn_with_dashes_from_path() {
    // IQN containing dashes (e.g. multi-word image names like "test-disk")
    let path = PathBuf::from(
        "/dev/disk/by-path/ip-127.0.0.1:3260-iscsi-iqn.2026-03.com.fyrastack:test-disk-lun-0",
    );
    let target = ISCSITarget::try_from(path).unwrap();
    assert_eq!(target.host, "127.0.0.1:3260");
    assert_eq!(target.iqn, "iqn.2026-03.com.fyrastack:test-disk");
    assert_eq!(target.lun, 0);
}

#[test]
fn test_iscsi_nonzero_lun_from_path() {
    // LUN > 0 must not be silently truncated to 0
    let path = PathBuf::from(
        "/dev/disk/by-path/ip-192.168.1.1:3260-iscsi-iqn.2026-03.com.fyrastack:boot-lun-2",
    );
    let target = ISCSITarget::try_from(path).unwrap();
    assert_eq!(target.host, "192.168.1.1:3260");
    assert_eq!(target.iqn, "iqn.2026-03.com.fyrastack:boot");
    assert_eq!(target.lun, 2);
}

#[test]
fn test_iscsi_nonzero_lun_from_uri() {
    // both numeric (/1) and prefixed (/lun1) URI forms for LUN > 0
    let uri_numeric =
        Url::parse("iscsi://target.example.com:3260/iqn.2026-03.com.fyrastack:boot/1").unwrap();
    let t1 = ISCSITarget::from(&uri_numeric);
    assert_eq!(t1.lun, 1);

    let uri_prefixed =
        Url::parse("iscsi://target.example.com:3260/iqn.2026-03.com.fyrastack:boot/lun1").unwrap();
    let t2 = ISCSITarget::from(&uri_prefixed);
    assert_eq!(t2.lun, 1);
}

#[test]
fn test_iscsi_device_path_roundtrip() {
    // device_path → TryFrom<PathBuf> → same ISCSITarget fields
    let original = ISCSITarget {
        host: "10.0.0.1:3260".to_string(),
        iqn: "iqn.2026-03.com.fyrastack:test-disk".to_string(),
        lun: 3,
    };
    let device_path = original.to_device_path();
    let parsed = ISCSITarget::try_from(PathBuf::from(&device_path)).unwrap();
    assert_eq!(parsed.host, original.host);
    assert_eq!(parsed.iqn, original.iqn);
    assert_eq!(parsed.lun, original.lun);
}
