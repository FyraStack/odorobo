//! iSCSI initiator transformer for storage backend
//! resolves iscsi:// URIs by logging into the iSCSI target and returning the local
//! block device path for the specified target and LUN, e.g. /dev/disk/by-path/ip-*
use super::{StorageAcquisition, StorageDriver};
use async_trait::async_trait;
use serde::Deserialize;
use stable_eyre::{Result, eyre::eyre};
use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

// Shared privately with RBD without changing storage/mod.rs.
#[path = "command.rs"]
pub(super) mod command;
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

    /// Sessions are shared across LUNs. Inspect sysfs, not just this LUN's
    /// by-path symlink, which may not exist yet for an already logged-in target.
    async fn session_exists(&self) -> Result<bool> {
        self.session_exists_at(Path::new("/sys/class")).await
    }

    async fn session_exists_at(&self, root: &Path) -> Result<bool> {
        Ok(self.session_target_at(root).await?.is_some())
    }

    async fn session_target(&self) -> Result<Option<Self>> {
        self.session_target_at(Path::new("/sys/class")).await
    }

    /// open-iscsi retains the node record's hostname in `persistent_address`
    /// even when login used an IP. This persistent portal is both the resource
    /// identity and the actual locator for udev by-path lookup and node logout.
    async fn session_target_at(&self, root: &Path) -> Result<Option<Self>> {
        let mut sessions = match tokio::fs::read_dir(root.join("iscsi_session")).await {
            Ok(sessions) => sessions,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let (host, port) = split_portal(&self.host)?;
        let mut matched: Option<(String, Self)> = None;
        while let Some(session) = sessions.next_entry().await? {
            let target = tokio::fs::read_to_string(session.path().join("targetname")).await?;
            if target.trim() != self.iqn {
                continue;
            }
            let name = session.file_name();
            let name = name.to_string_lossy();
            let Some(id) = name.strip_prefix("session") else {
                continue;
            };
            let prefix = format!("connection{id}:");
            let mut session_portal = None;
            let mut inconsistent_portals = false;
            let mut connections = tokio::fs::read_dir(root.join("iscsi_connection")).await?;
            while let Some(connection) = connections.next_entry().await? {
                if !connection
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&prefix)
                {
                    continue;
                }
                let address =
                    tokio::fs::read_to_string(connection.path().join("persistent_address")).await?;
                let connection_port =
                    tokio::fs::read_to_string(connection.path().join("persistent_port")).await?;
                let connection_port = connection_port.trim().parse::<u16>()?;
                let persistent_portal =
                    normalized_portal(&format_portal(address.trim(), connection_port))?;
                if let Some(previous) = &session_portal {
                    inconsistent_portals |= previous != &persistent_portal;
                } else {
                    session_portal = Some(persistent_portal);
                }
                if connection_port != port {
                    continue;
                }
                let runtime_address =
                    match tokio::fs::read_to_string(connection.path().join("address")).await {
                        Ok(address) => Some(address),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error.into()),
                    };
                if portal_matches(host, port, address.trim(), runtime_address.as_deref()).await? {
                    let actual = Self {
                        host: format_portal(address.trim(), connection_port),
                        iqn: self.iqn.clone(),
                        lun: self.lun,
                    };
                    if let Some((previous_id, previous)) = &matched {
                        if previous_id != id || previous.host != actual.host {
                            return Err(eyre!(
                                "Multiple iSCSI sessions match canonical portal {} and IQN {}",
                                self.host,
                                self.iqn
                            ));
                        }
                    } else {
                        matched = Some((id.to_owned(), actual));
                    }
                }
            }
            // Logout is session-wide. Do not accept two connection portals as
            // independently keyed resources of the same multi-connection session.
            if inconsistent_portals
                && matched
                    .as_ref()
                    .is_some_and(|(matched_id, _)| matched_id == id)
            {
                return Err(eyre!(
                    "iSCSI session {id} has inconsistent persistent portal identities; \
                     automatic acquisition and logout are blocked"
                ));
            }
        }
        Ok(matched.map(|(_, target)| target))
    }

    #[tracing::instrument(skip(self))]
    pub async fn attach(&self) -> Result<StorageAcquisition> {
        if let Some(acquisition) = self
            .existing_acquisition_at(Path::new("/sys/class"))
            .await?
        {
            return Ok(acquisition);
        }
        // do iscsiadm login to the target, then find the corresponding device path in /dev/disk/by-path
        info!(?self, "Attaching iSCSI target");

        let output = match command::run(
            Command::new("iscsiadm")
                .env("LC_ALL", "C")
                .args(["-m", "node", "-T", &self.iqn, "-p", &self.host, "--login"]),
            "iscsiadm login",
        )
        .await
        {
            Ok(output) => output,
            Err(error) => return error.into_acquisition(),
        };
        if output.status.success() {
            // iscsiadm can exit zero without logging in (e.g. the requested
            // session count was already present after our absence check). Only
            // an explicit successful login plus one matching session proves an
            // acquisition; presence alone is vulnerable to an external race.
            if !confirms_login(&output.stdout, &self.iqn) {
                return Ok(StorageAcquisition::uncertain(eyre!(
                    "iscsiadm exited successfully without confirming a new login"
                )));
            }
            return Ok(self.confirmed_acquisition_at(Path::new("/sys/class")).await);
        }
        let error = eyre!(
            "iscsiadm login failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        // Even exit 15 (session exists) cannot establish who created a session
        // after our absence check. Do not borrow or detach an ambiguous session.
        StorageAcquisition::after_failed_command(error, self.session_exists().await)
    }

    async fn existing_acquisition_at(&self, root: &Path) -> Result<Option<StorageAcquisition>> {
        let Some(session) = self.session_target_at(root).await? else {
            return Ok(None);
        };
        // Canonicalization happens before the registry reserves its key.
        // A session appearing in that gap must not be borrowed under a
        // different key; retrying resolution will find its persistent key.
        self.ensure_session_identity(&session)?;
        Ok(Some(StorageAcquisition::borrowed()))
    }

    async fn confirmed_acquisition_at(&self, root: &Path) -> StorageAcquisition {
        match self.session_target_at(root).await {
            Ok(Some(session)) => match self.ensure_session_identity(&session) {
                Ok(()) => StorageAcquisition::owned(),
                // Login may have used an existing hostname node record or
                // raced an alias acquisition. Never release a session under
                // a key different from the one reserved before login.
                Err(error) => StorageAcquisition::uncertain(error),
            },
            Ok(None) => StorageAcquisition::uncertain(eyre!(
                "iscsiadm login succeeded but no matching session is observable"
            )),
            Err(error) => StorageAcquisition::uncertain(
                error.wrap_err("iscsiadm login succeeded but session discovery failed"),
            ),
        }
    }

    fn ensure_session_identity(&self, session: &Self) -> Result<()> {
        if normalized_portal(&self.host)? != normalized_portal(&session.host)? {
            return Err(eyre!(
                "iSCSI session identity changed from {} to {}; refusing acquisition or \
                 logout under a different resource key",
                self.host,
                session.host
            ));
        }
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn detach(&self) -> Result<()> {
        // Allow retained cleanup to be retried after a session disappeared.
        let Some(session) = self.session_target().await? else {
            return Ok(());
        };
        self.ensure_session_identity(&session)?;
        info!(?session, "Detaching iSCSI target");
        let output = command::run(
            Command::new("iscsiadm").args([
                "-m",
                "node",
                "-T",
                &session.iqn,
                "-p",
                &session.host,
                "--logout",
            ]),
            "iscsiadm logout",
        )
        .await
        .map_err(command::CommandFailure::into_release_report)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if self.session_exists().await? {
                return Err(eyre!("iscsiadm logout failed: {stderr}"));
            }
        }
        Ok(())
    }
}

fn confirms_login(stdout: &[u8], iqn: &str) -> bool {
    let target = format!(", target: {iqn}, portal: ");
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| {
            line.starts_with("Login to [iface: ")
                && line.contains(&target)
                && line.ends_with("] successful.")
        })
        .count()
        == 1
}

const DNS_TIMEOUT: Duration = Duration::from_secs(5);

fn split_portal(portal: &str) -> Result<(&str, u16)> {
    let (host, port) = portal
        .rsplit_once(':')
        .ok_or_else(|| eyre!("Invalid iSCSI portal"))?;
    Ok((host.trim_matches(['[', ']']), port.parse()?))
}

fn format_portal(host: &str, port: u16) -> String {
    let host = host.trim_matches(['[', ']']);
    if host
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_ipv6())
    {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn normalized_portal(portal: &str) -> Result<String> {
    let (host, port) = split_portal(portal)?;
    let host = host
        .parse::<IpAddr>()
        .map_or_else(|_| host.to_ascii_lowercase(), |address| address.to_string());
    Ok(format_portal(&host, port))
}

async fn portal_addresses(host: &str, port: u16) -> Result<Vec<IpAddr>> {
    let host = host.trim_matches(['[', ']']);
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(vec![address]);
    }
    let addresses = tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| eyre!("iSCSI DNS lookup timed out for {host}"))??;
    let addresses: Vec<_> = addresses.map(|address| address.ip()).collect();
    if addresses.is_empty() {
        return Err(eyre!("No addresses found for iSCSI portal {host}"));
    }
    Ok(addresses)
}

async fn portal_matches(
    host: &str,
    port: u16,
    persistent: &str,
    runtime: Option<&str>,
) -> Result<bool> {
    let persistent = persistent.trim_matches(['[', ']']);
    if host.eq_ignore_ascii_case(persistent) {
        return Ok(true);
    }
    let addresses = portal_addresses(host, port).await?;
    let persistent_ip = persistent.parse::<IpAddr>().ok();
    // A numeric persistent portal and its redirected connected portal are
    // aliases of the same node. Both must canonicalize to the persistent key.
    if persistent_ip.is_some_and(|address| addresses.contains(&address)) {
        return Ok(true);
    }
    // A hostname node's current DNS is not evidence of its connected peer.
    // Its exact name remains usable even after DNS churn or lookup failure.
    if let Some(runtime) =
        runtime.and_then(|value| value.trim().trim_matches(['[', ']']).parse::<IpAddr>().ok())
    {
        return Ok(addresses.contains(&runtime));
    }
    if persistent_ip.is_none() {
        return Err(eyre!(
            "Cannot prove iSCSI alias identity for persistent hostname {persistent} \
             without a numeric connected address; use the exact persistent portal"
        ));
    }
    Ok(false)
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
        let host = format_portal(&host_ip, port);
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

pub struct ISCSIStorage;

impl ISCSIStorage {
    async fn canonical_uri_at(uri: &Url, root: &Path) -> Result<Url> {
        let target = ISCSITarget::from(uri);
        if target.iqn.is_empty() {
            return Err(eyre!("iSCSI URI must have a target IQN"));
        }
        if uri.host_str().is_none() {
            return Err(eyre!("iSCSI URI must have a portal host"));
        }
        // The persistent portal is stable across redirects/reconnects, unlike
        // runtime addresses or a hostname's current DNS. Before login retain
        // the requested portal: acquisition must confirm that this exact key
        // is still the session identity before it can report owned/borrowed.
        let session = target.session_target_at(root).await?;
        let portal = normalized_portal(&session.as_ref().unwrap_or(&target).host)?;
        let (host, port) = split_portal(&portal)?;
        let mut canonical = uri.clone();
        if let Ok(address) = host.parse::<IpAddr>() {
            canonical
                .set_ip_host(address)
                .map_err(|()| eyre!("Invalid iSCSI portal"))?;
        } else {
            canonical.set_host(Some(host))?;
        }
        canonical
            .set_port(Some(port))
            .map_err(|()| eyre!("Invalid iSCSI port"))?;
        canonical.set_query(None);
        canonical.set_fragment(None);
        Ok(canonical)
    }
}

#[async_trait]
impl StorageDriver for ISCSIStorage {
    fn scheme(&self) -> &'static str {
        "iscsi"
    }
    async fn canonical_uri(&self, uri: &Url) -> Result<Url> {
        Self::canonical_uri_at(uri, Path::new("/sys/class")).await
    }

    fn resource_key(&self, uri: &Url) -> Result<String> {
        let target = ISCSITarget::from(uri);
        Ok(format!("iscsi://{}/{}", target.host, target.iqn))
    }

    async fn acquire(&self, uri: &Url) -> Result<StorageAcquisition> {
        ISCSITarget::from(uri).attach().await
    }

    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        let target = ISCSITarget::from(uri);
        let session = target.session_target().await?.ok_or_else(|| {
            eyre!(
                "No iSCSI session found for portal {} and IQN {}",
                target.host,
                target.iqn
            )
        })?;
        target.ensure_session_identity(&session)?;
        Ok(session.to_device_path())
    }

    async fn release(&self, uri: &Url) -> Result<()> {
        let target = ISCSITarget::from(uri);
        target.detach().await
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ResourceState, StorageDriverTransformer, StorageOwnership};
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::Semaphore;

    #[test]
    fn resource_identity_is_session_not_lun_or_disk_id() {
        let backend = ISCSIStorage;
        let first = Url::parse("iscsi://127.0.0.1/iqn.test/0?id=first").unwrap();
        let second = Url::parse("iscsi://127.0.0.1:3260/iqn.test/lun1?id=second").unwrap();
        assert_eq!(
            backend.resource_key(&first).unwrap(),
            backend.resource_key(&second).unwrap()
        );
        let other = Url::parse("iscsi://127.0.0.1/iqn.other/0").unwrap();
        assert_ne!(
            backend.resource_key(&first).unwrap(),
            backend.resource_key(&other).unwrap()
        );
    }

    #[tokio::test]
    async fn detects_preexisting_session_even_without_requested_lun_device() {
        let root =
            std::env::temp_dir().join(format!("odorobo-iscsi-test-{}", ulid::Ulid::generate()));
        let session = root.join("iscsi_session/session17");
        let connection = root.join("iscsi_connection/connection17:0");
        tokio::fs::create_dir_all(&session).await.unwrap();
        tokio::fs::create_dir_all(&connection).await.unwrap();
        tokio::fs::write(session.join("targetname"), "iqn.test\n")
            .await
            .unwrap();
        tokio::fs::write(connection.join("persistent_address"), "127.0.0.1\n")
            .await
            .unwrap();
        tokio::fs::write(connection.join("persistent_port"), "3260\n")
            .await
            .unwrap();
        let mut target = ISCSITarget::from(&Url::parse("iscsi://127.0.0.1/iqn.test/99").unwrap());
        assert!(target.session_exists_at(&root).await.unwrap());
        target.host = "127.0.0.2:3260".to_owned();
        assert!(!target.session_exists_at(&root).await.unwrap());
        target.host = "127.0.0.1:3261".to_owned();
        assert!(!target.session_exists_at(&root).await.unwrap());
        target.host = "127.0.0.1:3260".to_owned();
        target.iqn = "iqn.other".to_owned();
        assert!(!target.session_exists_at(&root).await.unwrap());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    async fn synthetic_session(root: &Path, id: u32, persistent: &str, runtime: Option<&str>) {
        let session = root.join(format!("iscsi_session/session{id}"));
        let connection = root.join(format!("iscsi_connection/connection{id}:0"));
        tokio::fs::create_dir_all(&session).await.unwrap();
        tokio::fs::create_dir_all(&connection).await.unwrap();
        tokio::fs::write(session.join("targetname"), "iqn.test\n")
            .await
            .unwrap();
        tokio::fs::write(connection.join("persistent_address"), persistent)
            .await
            .unwrap();
        tokio::fs::write(connection.join("persistent_port"), "3260\n")
            .await
            .unwrap();
        if let Some(runtime) = runtime {
            tokio::fs::write(connection.join("address"), runtime)
                .await
                .unwrap();
        }
    }

    struct SyntheticStorage {
        root: PathBuf,
        persistent: &'static str,
        runtime: &'static str,
        logins: AtomicUsize,
        logouts: AtomicUsize,
        session_created: Semaphore,
        publish_login: Semaphore,
    }

    impl SyntheticStorage {
        fn new(persistent: &'static str, runtime: &'static str, paused: bool) -> Arc<Self> {
            Arc::new(Self {
                root: std::env::temp_dir()
                    .join(format!("odorobo-iscsi-alias-{}", ulid::Ulid::generate())),
                persistent,
                runtime,
                logins: AtomicUsize::new(0),
                logouts: AtomicUsize::new(0),
                session_created: Semaphore::new(0),
                publish_login: Semaphore::new(usize::from(!paused)),
            })
        }
    }

    #[async_trait]
    impl StorageDriver for Arc<SyntheticStorage> {
        fn scheme(&self) -> &'static str {
            "iscsi"
        }

        async fn canonical_uri(&self, uri: &Url) -> Result<Url> {
            ISCSIStorage::canonical_uri_at(uri, &self.root).await
        }

        fn resource_key(&self, uri: &Url) -> Result<String> {
            ISCSIStorage.resource_key(uri)
        }

        async fn acquire(&self, uri: &Url) -> Result<StorageAcquisition> {
            let target = ISCSITarget::from(uri);
            if let Some(acquisition) = target.existing_acquisition_at(&self.root).await? {
                return Ok(acquisition);
            }
            self.logins.fetch_add(1, Ordering::SeqCst);
            synthetic_session(&self.root, 17, self.persistent, Some(self.runtime)).await;
            self.session_created.add_permits(1);
            self.publish_login.acquire().await.unwrap().forget();
            Ok(target.confirmed_acquisition_at(&self.root).await)
        }

        async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
            let target = ISCSITarget::from(uri);
            let session = target.session_target_at(&self.root).await?.unwrap();
            target.ensure_session_identity(&session)?;
            Ok(session.to_device_path())
        }

        async fn release(&self, uri: &Url) -> Result<()> {
            let target = ISCSITarget::from(uri);
            let session = target.session_target_at(&self.root).await?.unwrap();
            target.ensure_session_identity(&session)?;
            self.logouts.fetch_add(1, Ordering::SeqCst);
            tokio::fs::remove_dir_all(self.root.join("iscsi_session/session17")).await?;
            tokio::fs::remove_dir_all(self.root.join("iscsi_connection/connection17:0")).await?;
            Ok(())
        }
    }

    fn synthetic_transformer(state: &Arc<SyntheticStorage>) -> Arc<StorageDriverTransformer> {
        Arc::new(StorageDriverTransformer {
            registry: Arc::default(),
            ..StorageDriverTransformer::new().with_backend(Arc::clone(state))
        })
    }

    async fn resolve_synthetic(
        transform: &StorageDriverTransformer,
        vmid: &str,
        uri: &str,
    ) -> Result<PathBuf> {
        let uri = Url::parse(uri).unwrap();
        transform
            .resolve_disk(vmid, &uri, transform.find_backend(&uri).unwrap())
            .await
    }

    #[tokio::test]
    async fn redirected_owner_and_alias_share_the_key_reserved_before_login() {
        let state = SyntheticStorage::new("192.0.2.17", "192.0.2.18", false);
        let transform = synthetic_transformer(&state);
        let owner_uri = Url::parse("iscsi://192.0.2.17/iqn.test/0?id=owner").unwrap();
        let before_login = ISCSIStorage::canonical_uri_at(&owner_uri, &state.root)
            .await
            .unwrap();
        let reserved_key = ISCSIStorage.resource_key(&before_login).unwrap();
        resolve_synthetic(&transform, "owner", owner_uri.as_str())
            .await
            .unwrap();
        let alias = "iscsi://192.0.2.18:3260/iqn.test/lun1?id=borrower";
        let path = resolve_synthetic(&transform, "borrower", alias)
            .await
            .unwrap();
        assert_eq!(
            path,
            PathBuf::from("/dev/disk/by-path/ip-192.0.2.17:3260-iscsi-iqn.test-lun-1")
        );
        {
            let ledger = transform.registry.lock().await;
            assert_eq!(ledger.resources.len(), 1);
            assert_eq!(ledger.resources[&reserved_key].references, 2);
            assert!(matches!(
                ledger.resources[&reserved_key].ownership,
                StorageOwnership::Owned
            ));
            assert_eq!(ledger.leases["owner"], ledger.leases["borrower"]);
            drop(ledger);
        };
        transform.release_vm("owner").await.unwrap();
        assert_eq!(state.logouts.load(Ordering::SeqCst), 0);
        // Dropping the owner's reference must not unmap the alias borrower's LUN.
        assert_eq!(
            resolve_synthetic(&transform, "third", alias).await.unwrap(),
            path
        );
        transform.release_vm("third").await.unwrap();
        assert_eq!(state.logouts.load(Ordering::SeqCst), 0);
        transform.release_vm("borrower").await.unwrap();
        assert_eq!(state.logouts.load(Ordering::SeqCst), 1);
        assert_eq!(state.logins.load(Ordering::SeqCst), 1);
        assert!(transform.registry.lock().await.resources.is_empty());
        tokio::fs::remove_dir_all(&state.root).await.unwrap();
    }

    #[tokio::test]
    async fn runtime_alias_joins_acquiring_owner_even_after_owner_waiter_cancels() {
        let state = SyntheticStorage::new("192.0.2.17", "192.0.2.18", true);
        let transform = synthetic_transformer(&state);
        let owner_transform = Arc::clone(&transform);
        let owner = tokio::spawn(async move {
            resolve_synthetic(&owner_transform, "owner", "iscsi://192.0.2.17/iqn.test/0").await
        });
        tokio::time::timeout(Duration::from_secs(5), state.session_created.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        owner.abort();
        drop(owner.await);
        let borrower_transform = Arc::clone(&transform);
        let mut borrower = tokio::spawn(async move {
            resolve_synthetic(
                &borrower_transform,
                "borrower",
                "iscsi://192.0.2.18/iqn.test/1",
            )
            .await
        });
        // The alias must wait for ownership publication, not create a borrowed
        // resource while the owner's login task is still acquiring.
        let _elapsed = tokio::time::timeout(Duration::from_millis(25), &mut borrower)
            .await
            .unwrap_err();
        {
            let ledger = transform.registry.lock().await;
            assert_eq!(ledger.resources.len(), 1);
            assert!(matches!(
                ledger.resources.values().next().unwrap().state,
                ResourceState::Acquiring(_)
            ));
            drop(ledger);
        };
        state.publish_login.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), borrower)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        transform.release_vm("owner").await.unwrap();
        assert_eq!(state.logouts.load(Ordering::SeqCst), 0);
        transform.release_vm("borrower").await.unwrap();
        assert_eq!(state.logouts.load(Ordering::SeqCst), 1);
        assert_eq!(state.logins.load(Ordering::SeqCst), 1);
        tokio::fs::remove_dir_all(&state.root).await.unwrap();
    }

    #[tokio::test]
    async fn external_redirected_session_is_borrowed_under_one_persistent_key() {
        let state = SyntheticStorage::new("192.0.2.17", "192.0.2.18", false);
        synthetic_session(&state.root, 17, state.persistent, Some(state.runtime)).await;
        let transform = synthetic_transformer(&state);
        resolve_synthetic(&transform, "first", "iscsi://192.0.2.17/iqn.test/0")
            .await
            .unwrap();
        resolve_synthetic(&transform, "second", "iscsi://192.0.2.18/iqn.test/1")
            .await
            .unwrap();
        {
            let ledger = transform.registry.lock().await;
            assert_eq!(ledger.resources.len(), 1);
            let resource = ledger.resources.values().next().unwrap();
            assert_eq!(resource.references, 2);
            assert!(matches!(resource.ownership, StorageOwnership::Borrowed));
            drop(ledger);
        };
        transform.release_vm("first").await.unwrap();
        transform.release_vm("second").await.unwrap();
        assert_eq!(state.logins.load(Ordering::SeqCst), 0);
        assert_eq!(state.logouts.load(Ordering::SeqCst), 0);
        assert!(
            ISCSITarget::from(&Url::parse("iscsi://192.0.2.17/iqn.test/0").unwrap())
                .session_exists_at(&state.root)
                .await
                .unwrap()
        );
        tokio::fs::remove_dir_all(&state.root).await.unwrap();
    }

    #[tokio::test]
    async fn hostname_owner_and_connected_alias_keep_the_prelogin_hostname_key() {
        // This name intentionally has no DNS record. The actual sysfs node
        // identity, not fresh DNS, must drive alias lookup and final cleanup.
        let state = SyntheticStorage::new("missing-odorobo-node.invalid", "192.0.2.18", false);
        let transform = synthetic_transformer(&state);
        let owner = "iscsi://missing-odorobo-node.invalid/iqn.test/0";
        let before = ISCSIStorage::canonical_uri_at(&Url::parse(owner).unwrap(), &state.root)
            .await
            .unwrap();
        resolve_synthetic(&transform, "owner", owner).await.unwrap();
        let alias = "iscsi://192.0.2.18/iqn.test/1";
        resolve_synthetic(&transform, "borrower", alias)
            .await
            .unwrap();
        let after = ISCSIStorage::canonical_uri_at(&Url::parse(alias).unwrap(), &state.root)
            .await
            .unwrap();
        assert_eq!(
            ISCSIStorage.resource_key(&before).unwrap(),
            ISCSIStorage.resource_key(&after).unwrap()
        );
        transform.release_vm("owner").await.unwrap();
        assert_eq!(state.logouts.load(Ordering::SeqCst), 0);
        transform.release_vm("borrower").await.unwrap();
        assert_eq!(state.logouts.load(Ordering::SeqCst), 1);
        tokio::fs::remove_dir_all(&state.root).await.unwrap();
    }

    #[tokio::test]
    async fn dns_normalizes_hostname_session_but_preserves_actual_locator() {
        let root =
            std::env::temp_dir().join(format!("odorobo-iscsi-dns-{}", ulid::Ulid::generate()));
        synthetic_session(&root, 17, "localhost\n", Some("127.0.0.1\n")).await;
        let target = ISCSITarget::from(&Url::parse("iscsi://127.0.0.1/iqn.test/99").unwrap());
        let session = target.session_target_at(&root).await.unwrap().unwrap();
        assert_eq!(session.host, "localhost:3260");
        assert_eq!(
            session.to_device_path(),
            PathBuf::from("/dev/disk/by-path/ip-localhost:3260-iscsi-iqn.test-lun-99")
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn identity_change_between_canonicalization_and_acquisition_is_not_borrowed_or_owned() {
        let root =
            std::env::temp_dir().join(format!("odorobo-iscsi-key-race-{}", ulid::Ulid::generate()));
        let requested = Url::parse("iscsi://192.0.2.17/iqn.test/0").unwrap();
        let before = ISCSIStorage::canonical_uri_at(&requested, &root)
            .await
            .unwrap();
        // An existing node may retain a hostname although login was requested
        // numerically. Without an alias ledger this is not the reserved key.
        synthetic_session(
            &root,
            17,
            "missing-odorobo-node.invalid",
            Some("192.0.2.17"),
        )
        .await;
        let target = ISCSITarget::from(&before);
        assert!(
            target
                .existing_acquisition_at(&root)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("session identity changed")
        );
        let after_login = target.confirmed_acquisition_at(&root).await;
        assert!(matches!(
            after_login.ownership,
            StorageOwnership::Uncertain(_)
        ));
        let after = ISCSIStorage::canonical_uri_at(&requested, &root)
            .await
            .unwrap();
        assert_ne!(
            ISCSIStorage.resource_key(&before).unwrap(),
            ISCSIStorage.resource_key(&after).unwrap()
        );
        let canonical_target = ISCSITarget::from(&after);
        let borrowed = canonical_target
            .existing_acquisition_at(&root)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(borrowed.ownership, StorageOwnership::Borrowed));
        // Retrying or teardown with the stale key must not detach this session.
        assert!(target.ensure_session_identity(&canonical_target).is_err());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn hostname_alias_without_connected_identity_is_rejected_not_resolved_by_fresh_dns() {
        let root = std::env::temp_dir().join(format!(
            "odorobo-iscsi-no-runtime-{}",
            ulid::Ulid::generate()
        ));
        synthetic_session(&root, 17, "localhost", None).await;
        let numeric = Url::parse("iscsi://127.0.0.1/iqn.test/0").unwrap();
        assert!(
            ISCSIStorage::canonical_uri_at(&numeric, &root)
                .await
                .unwrap_err()
                .to_string()
                .contains("Cannot prove iSCSI alias identity")
        );
        let hostname = Url::parse("iscsi://localhost/iqn.test/0").unwrap();
        let canonical = ISCSIStorage::canonical_uri_at(&hostname, &root)
            .await
            .unwrap();
        assert_eq!(canonical.host_str(), Some("localhost"));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn runtime_address_survives_dns_churn_and_lookup_failure() {
        let root =
            std::env::temp_dir().join(format!("odorobo-iscsi-runtime-{}", ulid::Ulid::generate()));
        // localhost resolves to loopback, not the runtime address. It must not
        // cause the old session to be misattributed to a new canonical IP.
        synthetic_session(&root, 17, "localhost\n", Some("192.0.2.17\n")).await;
        let old = ISCSITarget::from(&Url::parse("iscsi://192.0.2.17/iqn.test/0").unwrap());
        assert_eq!(
            old.session_target_at(&root).await.unwrap().unwrap().host,
            "localhost:3260"
        );
        let new = ISCSITarget::from(&Url::parse("iscsi://127.0.0.1/iqn.test/0").unwrap());
        assert!(!new.session_exists_at(&root).await.unwrap());
        // A vanished DNS record must still resolve and logout via the exact
        // node portal. No lookup of this .invalid hostname is needed.
        tokio::fs::write(
            root.join("iscsi_connection/connection17:0/persistent_address"),
            "missing-odorobo-node.invalid\n",
        )
        .await
        .unwrap();
        let session = old.session_target_at(&root).await.unwrap().unwrap();
        assert_eq!(session.host, "missing-odorobo-node.invalid:3260");
        assert_eq!(
            session.to_device_path(),
            PathBuf::from(
                "/dev/disk/by-path/ip-missing-odorobo-node.invalid:3260-iscsi-iqn.test-lun-0"
            )
        );
        assert!(session.session_exists_at(&root).await.unwrap());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn unresolved_session_without_runtime_address_is_not_confirmed_absence() {
        let root =
            std::env::temp_dir().join(format!("odorobo-iscsi-unknown-{}", ulid::Ulid::generate()));
        synthetic_session(&root, 17, "missing-odorobo-node.invalid\n", None).await;
        let target = ISCSITarget::from(&Url::parse("iscsi://127.0.0.1/iqn.test/0").unwrap());
        drop(target.session_exists_at(&root).await.unwrap_err());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn different_persistent_portals_in_one_session_cannot_create_separate_keys() {
        let root = std::env::temp_dir().join(format!(
            "odorobo-iscsi-multi-connection-{}",
            ulid::Ulid::generate()
        ));
        synthetic_session(&root, 17, "192.0.2.17", Some("192.0.2.18")).await;
        let second = root.join("iscsi_connection/connection17:1");
        tokio::fs::create_dir_all(&second).await.unwrap();
        tokio::fs::write(second.join("persistent_address"), "192.0.2.19")
            .await
            .unwrap();
        tokio::fs::write(second.join("persistent_port"), "3260")
            .await
            .unwrap();
        tokio::fs::write(second.join("address"), "192.0.2.20")
            .await
            .unwrap();
        for host in ["192.0.2.17", "192.0.2.18", "192.0.2.19", "192.0.2.20"] {
            let uri = Url::parse(&format!("iscsi://{host}/iqn.test/0")).unwrap();
            assert!(
                ISCSIStorage::canonical_uri_at(&uri, &root)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("inconsistent persistent portal identities")
            );
        }
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn multiple_matching_sessions_are_ambiguous_not_owned() {
        let root =
            std::env::temp_dir().join(format!("odorobo-iscsi-multiple-{}", ulid::Ulid::generate()));
        synthetic_session(&root, 17, "127.0.0.1\n", Some("127.0.0.1\n")).await;
        synthetic_session(&root, 18, "localhost\n", Some("127.0.0.1\n")).await;
        let target = ISCSITarget::from(&Url::parse("iscsi://127.0.0.1/iqn.test/0").unwrap());
        assert!(
            target
                .session_target_at(&root)
                .await
                .unwrap_err()
                .to_string()
                .contains("Multiple iSCSI sessions")
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[test]
    fn zero_exit_or_session_presence_alone_does_not_prove_new_login() {
        let success = b"Logging in to target...\nLogin to [iface: default, target: iqn.test, portal: 127.0.0.1,3260] successful.\n";
        assert!(confirms_login(success, "iqn.test"));
        assert!(!confirms_login(success, "iqn.other"));
        assert!(!confirms_login(
            b"iscsiadm: 1 session requested, but 1 already present.\n",
            "iqn.test"
        ));
        assert!(!confirms_login(b"", "iqn.test"));
        let multiple = [success.as_slice(), success.as_slice()].concat();
        assert!(!confirms_login(&multiple, "iqn.test"));
    }

    #[test]
    fn ipv6_portals_preserve_brackets_for_device_and_cli() {
        let target = ISCSITarget::from(&Url::parse("iscsi://[::1]/iqn.test/0").unwrap());
        assert_eq!(target.host, "[::1]:3260");
        assert_eq!(split_portal(&target.host).unwrap(), ("::1", 3260));
        assert_eq!(format_portal("::1", 3260), "[::1]:3260");
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
