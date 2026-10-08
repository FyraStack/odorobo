//! iSCSI initiator transformer for storage backend
//! resolves iscsi:// URIs by logging into the iSCSI target and returning the local
//! block device path for the specified target and LUN, e.g. /dev/disk/by-path/ip-*
use super::StorageDriver;
use async_trait::async_trait;
use serde::Deserialize;
use stable_eyre::{Result, eyre::eyre};
use std::path::PathBuf;
use tokio::process::Command;
use tracing::info;
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

        let output = Command::new("iscsiadm")
            .args(["-m", "node", "-T", &self.iqn, "-p", &self.host, "--login"])
            .output()
            .await
            .map_err(|e| eyre!("Failed to execute iscsiadm command: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("iscsiadm login failed: {stderr}"));
        }
        Ok(self.to_device_path())
    }

    #[tracing::instrument(skip(self))]
    pub async fn detach(&self) -> Result<()> {
        info!(?self, "Detaching iSCSI target");
        let output = Command::new("iscsiadm")
            .args(["-m", "node", "-T", &self.iqn, "-p", &self.host, "--logout"])
            .output()
            .await
            .map_err(|e| eyre!("Failed to execute iscsiadm command: {e}"))?;
        // open-iscsi exit 21 means no matching session: failed login may
        // legitimately leave nothing to detach.
        if !output.status.success() && output.status.code() != Some(21) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("iscsiadm logout failed: {stderr}"));
        }
        Ok(())
    }
}

impl From<ISCSITarget> for PathBuf {
    fn from(val: ISCSITarget) -> Self {
        val.to_device_path()
    }
}

impl From<&Url> for ISCSITarget {
    fn from(uri: &Url) -> Self {
        let host_ip = uri.host_str().unwrap_or_default().to_owned();
        let port = uri.port().unwrap_or(3260);
        let host = format!("{host_ip}:{port}");
        let mut path_segments = uri.path_segments().into_iter().flatten();
        let iqn = path_segments.next().unwrap_or_default().to_owned();
        let lun_str = path_segments.next().unwrap_or_default();
        let lun = lun_str
            .strip_prefix("lun")
            .unwrap_or(lun_str)
            .parse::<u32>()
            .unwrap_or(0);
        Self { host, iqn, lun }
    }
}

// fn list_iscsi_devices() -> Result<Vec<PathBuf>> {
//     // simply just list /dev/disk/by-path/ip-* for now

//     let mut devices = Vec::new();
//     let by_path = Path::new("/dev/disk/by-path");
//     if by_path.exists() {
//         for entry in by_path.read_dir()? {
//             let entry = entry?;
//             let file_name = entry.file_name();
//             let file_name_str = file_name.to_string_lossy();
//             if file_name_str.starts_with("ip-") {
//                 let device_path = entry.path();
//                 if device_path.exists() {
//                     devices.push(device_path);
//                 }
//             }
//         }
//     }
//     Ok(devices)
// }

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
        let host = host_part.to_owned();
        let iqn = iqn_part.to_owned();
        let lun = lun_part.parse::<u32>().unwrap_or(0);
        Ok(Self { host, iqn, lun })
    }
}

fn target_sessions(output: &str, target: &ISCSITarget) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.get(2)?.split(',').next() != Some(target.host.as_str())
                || *fields.get(3)? != target.iqn
            {
                return None;
            }
            fields.get(1)?.trim_matches(['[', ']']).parse().ok()
        })
        .collect()
}

#[cfg(test)]
fn session_matches(output: &str, target: &ISCSITarget) -> bool {
    !target_sessions(output, target).is_empty()
}

static SESSION_OPERATIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static SESSION_RESERVATIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<(String, String)>>,
> = std::sync::LazyLock::new(Default::default);

async fn sessions(target: &ISCSITarget) -> Result<Vec<u32>> {
    let output = Command::new("iscsiadm")
        .args(["-m", "session"])
        .output()
        .await?;
    if output.status.code() == Some(21) {
        return Ok(vec![]);
    }
    if !output.status.success() {
        return Err(eyre!("cannot establish iSCSI session ownership"));
    }
    Ok(target_sessions(
        &String::from_utf8_lossy(&output.stdout),
        target,
    ))
}

struct SessionOwnership {
    target: ISCSITarget,
    ids: Option<Vec<u32>>,
}

async fn canonical_target(uri: &Url) -> Result<ISCSITarget> {
    let mut target = ISCSITarget::from(uri);
    let port = uri.port().unwrap_or(3260);
    let addresses = match uri.host().ok_or_else(|| eyre!("missing iSCSI host"))? {
        url::Host::Ipv4(ip) => vec![std::net::SocketAddr::new(ip.into(), port)],
        url::Host::Ipv6(ip) => vec![std::net::SocketAddr::new(ip.into(), port)],
        url::Host::Domain(host) => tokio::net::lookup_host((host, port)).await?.collect(),
    };
    let address = addresses
        .first()
        .ok_or_else(|| eyre!("iSCSI host resolved no addresses"))?;
    // Use the same numeric portal for login and session identity queries.
    target.host = address.to_string();
    Ok(target)
}

#[derive(Default)]
pub struct ISCSIStorage {
    owned: tokio::sync::Mutex<std::collections::HashMap<String, SessionOwnership>>,
}

#[async_trait]
impl StorageDriver for ISCSIStorage {
    fn scheme(&self) -> &'static str {
        "iscsi"
    }
    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        let _operation = SESSION_OPERATIONS.lock().await;
        let target = canonical_target(uri).await?;
        let identity = (target.host.clone(), target.iqn.clone());
        if SESSION_RESERVATIONS.lock().unwrap().contains(&identity)
            || !sessions(&target).await?.is_empty()
        {
            return Err(eyre!(
                "iSCSI target already has a session; refusing adoption"
            ));
        }
        SESSION_RESERVATIONS.lock().unwrap().insert(identity);
        self.owned.lock().await.insert(
            uri.as_str().to_owned(),
            SessionOwnership {
                target: target.clone(),
                ids: None,
            },
        );
        let result = target.attach().await;
        // Discover sessions even on partial login failure while the global
        // acquisition/release lock excludes competing VM backend instances.
        let ids = sessions(&target).await?;
        self.owned.lock().await.get_mut(uri.as_str()).unwrap().ids = Some(ids);
        result
    }
    async fn release(&self, uri: &Url) -> Result<()> {
        let _operation = SESSION_OPERATIONS.lock().await;
        let mut owned = self.owned.lock().await;
        let Some(ownership) = owned.get_mut(uri.as_str()) else {
            return Ok(());
        };
        let current = sessions(&ownership.target).await?;
        // Pending discovery must be reconciled, never mistaken for no owned
        // session. A failed query preserves this pending token for retry.
        let ids = ownership.ids.get_or_insert_with(|| current.clone());
        while let Some(id) = ids.last().copied() {
            if current.contains(&id) {
                let output = Command::new("iscsiadm")
                    .args(["-m", "session", "-r", &id.to_string(), "--logout"])
                    .output()
                    .await?;
                if !output.status.success() && output.status.code() != Some(21) {
                    return Err(eyre!("owned iSCSI session logout failed"));
                }
            }
            ids.pop();
        }
        SESSION_RESERVATIONS
            .lock()
            .unwrap()
            .remove(&(ownership.target.host.clone(), ownership.target.iqn.clone()));
        owned.remove(uri.as_str());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn numeric_portals_canonicalize_without_dns() {
        assert_eq!(
            canonical_target(&Url::parse("iscsi://127.0.0.1/iqn/0").unwrap())
                .await
                .unwrap()
                .host,
            "127.0.0.1:3260"
        );
        assert_eq!(
            canonical_target(&Url::parse("iscsi://[::1]/iqn/0").unwrap())
                .await
                .unwrap()
                .host,
            "[::1]:3260"
        );
    }

    #[test]
    fn session_identity_is_not_a_substring_match() {
        let target = ISCSITarget {
            host: "127.0.0.1:3260".into(),
            iqn: "iqn.fixture".into(),
            lun: 0,
        };
        assert!(!session_matches(
            "tcp: [1] 127.0.0.1:3260,1 iqn.fixture-extra",
            &target
        ));
        assert!(session_matches(
            "tcp: [1] 127.0.0.1:3260,1 iqn.fixture",
            &target
        ));
        assert!(!session_matches(
            "tcp: [1] 127.0.0.11:3260,1 iqn.fixture",
            &target
        ));
    }

    #[tokio::test]
    async fn failed_unowned_login_cleanup_is_a_noop() {
        ISCSIStorage::default()
            .release(&Url::parse("iscsi://unreachable.example/iqn.fixture/0").unwrap())
            .await
            .unwrap();
    }

    #[test]
    fn test_iscsi_target_parsing_from_path() {
        let path = PathBuf::from(
            "/dev/disk/by-path/ip-127.0.0.1:3260-iscsi-iqn.2026-03.com.fyrastack:test-lun-0",
        );
        let target = ISCSITarget::try_from(path).unwrap();
        println!("{target:?}");

        assert_eq!(target.host, "127.0.0.1:3260");
        assert_eq!(target.iqn, "iqn.2026-03.com.fyrastack:test");
        assert_eq!(target.lun, 0);
    }

    #[test]
    fn test_iscsi_target_parsing() {
        let uri =
            Url::parse("iscsi://target.example.com:3260/iqn.2024-01.com.example:target1/lun0")
                .unwrap();
        let target = ISCSITarget::from(&uri);
        println!("{target:?}");
        assert_eq!(target.host, "target.example.com:3260");
        assert_eq!(target.iqn, "iqn.2024-01.com.example:target1");
        assert_eq!(target.lun, 0);
        let uri2 =
            Url::parse("iscsi://target.example.com/iqn.2024-01.com.example:target1/0").unwrap();
        let target2 = ISCSITarget::from(&uri2);
        println!("{target2:?}");
        assert_eq!(target2.host, "target.example.com:3260");
        assert_eq!(target2.iqn, "iqn.2024-01.com.example:target1");
        assert_eq!(target2.lun, 0);
    }

    #[test]
    fn test_iscsi_lossless_conversion() {
        let uri = Url::parse("iscsi://target.example.com:3260/iqn.2024-01.com.example:target1/0")
            .unwrap();
        let target = ISCSITarget::from(&uri);
        let reconstructed_uri = format!("iscsi://{}/{}/{}", target.host, target.iqn, target.lun);
        assert_eq!(uri.as_str(), reconstructed_uri);
    }

    #[test]
    fn test_iscsi_uri_to_device_path() {
        let uri =
            Url::parse("iscsi://target.example.com:3260/iqn.2026-03.com.fyrastack:test/0").unwrap();
        let actual_device_path = "/dev/disk/by-path/ip-target.example.com:3260-iscsi-iqn.2026-03.com.fyrastack:test-lun-0";
        let target = ISCSITarget::from(&uri);
        let device_path = target.to_device_path().display().to_string();
        println!("Device path: {device_path}");
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
            Url::parse("iscsi://target.example.com:3260/iqn.2026-03.com.fyrastack:boot/lun1")
                .unwrap();
        let t2 = ISCSITarget::from(&uri_prefixed);
        assert_eq!(t2.lun, 1);
    }

    #[test]
    fn test_iscsi_device_path_roundtrip() {
        // device_path → TryFrom<PathBuf> → same ISCSITarget fields
        let original = ISCSITarget {
            host: "10.0.0.1:3260".to_owned(),
            iqn: "iqn.2026-03.com.fyrastack:test-disk".to_owned(),
            lun: 3,
        };
        let device_path = original.to_device_path();
        let parsed = ISCSITarget::try_from(PathBuf::from(&device_path)).unwrap();
        assert_eq!(parsed.host, original.host);
        assert_eq!(parsed.iqn, original.iqn);
        assert_eq!(parsed.lun, original.lun);
    }
}
