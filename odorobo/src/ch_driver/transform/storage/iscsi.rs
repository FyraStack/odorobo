//! iSCSI initiator transformer for storage backend
//! resolves iscsi:// URIs by logging into the iSCSI target and returning the local
//! block device path for the specified target and LUN, e.g. /dev/disk/by-path/ip-*
use super::{StorageDriver, run_storage_command, run_storage_command_with_timeout};
use crate::ch_driver::transform::StorageReleaseState;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use stable_eyre::{
    Result,
    eyre::{WrapErr, eyre},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::IpAddr,
    path::PathBuf,
    pin::Pin,
    time::Duration,
};
use tokio::process::Command;
use tracing::info;
use url::Url;

const SESSION_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(5);

type ResolverFuture<'a> = Pin<Box<dyn Future<Output = Result<BTreeSet<IpAddr>>> + Send + 'a>>;

trait PortalResolver: Sync {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolverFuture<'a>;
}

struct SystemPortalResolver;

impl PortalResolver for SystemPortalResolver {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolverFuture<'a> {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host, port))
                .await
                .map_err(|error| eyre!("Failed to resolve iSCSI portal {host}: {error}"))?
                .map(|address| address.ip())
                .collect::<BTreeSet<_>>();
            if addresses.is_empty() {
                return Err(eyre!("iSCSI portal {host} resolved to no addresses"));
            }
            Ok(addresses)
        })
    }
}

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
        Err(eyre!(
            "iSCSI login requires the durable storage ownership API; refusing an unjournaled session"
        ))
    }

    async fn attach_with_identity(
        &self,
        persist_identity: &mut (dyn for<'a> FnMut(&'a str) -> Result<()> + Send),
    ) -> Result<PathBuf> {
        self.attach_with_identity_and_timeout(persist_identity, super::STORAGE_COMMAND_TIMEOUT)
            .await
    }

    async fn attach_with_identity_and_timeout(
        &self,
        persist_identity: &mut (dyn for<'a> FnMut(&'a str) -> Result<()> + Send),
        login_timeout: Duration,
    ) -> Result<PathBuf> {
        // Pin the requested hostname to a bounded numeric endpoint set before
        // login, then correlate both the precheck and result against that set.
        let requested_ips = self.resolve_portal_addresses().await?;
        self.ensure_unattached_at(&requested_ips).await?;
        let before = Self::session_output().await?;
        let before_ids = parse_sessions(&before)?
            .into_iter()
            .map(|session| session.session_id)
            .collect::<BTreeSet<_>>();

        let pending = PendingISCSISession {
            iqn: self.iqn.clone(),
            requested_portal: parse_portal(&self.host)?,
        };
        let pending = format!(
            "pending:{}",
            serde_json::to_string(&pending).wrap_err("Failed to serialize pending iSCSI login")?
        );
        // Record submission uncertainty durably before invoking iscsiadm: its
        // daemon may complete the login after this CLI is killed or the agent
        // exits, even when no session is visible at the immediate query.
        persist_identity(&pending)?;

        info!(?self, "Attaching iSCSI target");
        let mut command = Command::new("iscsiadm");
        command.args(["-m", "node", "-T", &self.iqn, "-p", &self.host, "--login"]);
        let output =
            run_storage_command_with_timeout(&mut command, "iscsiadm login", login_timeout).await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("iscsiadm login failed: {stderr}"));
        }

        // Persist the actual newly created kernel session identity (numeric
        // portal and session id), never a hostname that can later resolve to a
        // different target. Ambiguous acquisition remains claimed for recovery.
        let after = Self::session_output().await?;
        let new_sessions = newly_acquired_sessions(
            &after,
            &before_ids,
            &self.iqn,
            &parse_portal(&self.host)?,
            &requested_ips,
        )?;
        if new_sessions.len() != 1 {
            return Err(eyre!(
                "Could not identify exactly one newly acquired iSCSI session for {} (found {})",
                self.iqn,
                new_sessions.len()
            ));
        }
        let session = new_sessions
            .into_iter()
            .next()
            .expect("one session checked");
        let pinned = PinnedISCSISession {
            session_id: session.session_id,
            portal: session.portal,
            iqn: session.iqn,
        };
        persist_identity(
            &serde_json::to_string(&pinned)
                .wrap_err("Failed to serialize acquired iSCSI session identity")?,
        )?;
        Ok(self.to_device_path())
    }

    async fn session_output() -> Result<String> {
        let deadline = tokio::time::Instant::now()
            .checked_add(SESSION_VERIFICATION_TIMEOUT)
            .ok_or_else(|| eyre!("iSCSI session verification deadline overflow"))?;
        let mut command = Command::new("iscsiadm");
        command.args(["-m", "session"]);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let output =
            run_storage_command_with_timeout(&mut command, "iscsiadm session query", remaining)
                .await?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            let message = format!("{stdout}{stderr}");
            if message.to_ascii_lowercase().contains("no active sessions") {
                return Ok(String::new());
            }
            return Err(eyre!("Failed to query iSCSI sessions: {message}"));
        }
        if tokio::time::Instant::now() > deadline {
            return Err(eyre!("Timed out querying iSCSI session state"));
        }
        Ok(stdout)
    }

    async fn resolve_portal_addresses(&self) -> Result<BTreeSet<IpAddr>> {
        let portal = parse_portal(&self.host)?;
        tokio::time::timeout(
            SESSION_VERIFICATION_TIMEOUT,
            portal_ip_addresses(&portal, &mut BTreeMap::new(), &SystemPortalResolver),
        )
        .await
        .map_err(|_| {
            eyre!(
                "Timed out resolving iSCSI portal after {:?}",
                SESSION_VERIFICATION_TIMEOUT
            )
        })?
    }

    async fn ensure_unattached_at(&self, requested_ips: &BTreeSet<IpAddr>) -> Result<()> {
        let output = Self::session_output().await?;
        let requested_portal = parse_portal(&self.host)?;
        let has_session = parse_sessions(&output)?.into_iter().any(|session| {
            session.iqn == self.iqn
                && session.portal.port == requested_portal.port
                && session
                    .portal
                    .host
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| requested_ips.contains(&ip))
        });
        if has_session {
            return Err(eyre!(
                "iSCSI target {} is already active; cross-VM session sharing is unsupported",
                self.iqn
            ));
        }
        Ok(())
    }

    async fn ensure_unattached(&self) -> Result<()> {
        let requested_ips = self.resolve_portal_addresses().await?;
        self.ensure_unattached_at(&requested_ips).await
    }

    async fn detach_pinned(&self, attachment: &str) -> Result<()> {
        let pinned: PinnedISCSISession = serde_json::from_str(attachment)
            .wrap_err("Invalid persisted iSCSI session identity")?;
        if pinned.iqn != self.iqn {
            return Err(eyre!("Pinned iSCSI session belongs to a different IQN"));
        }
        let before = parse_sessions(&Self::session_output().await?)?;
        let Some(active) = before
            .iter()
            .find(|session| session.session_id == pinned.session_id)
        else {
            info!(session_id = %pinned.session_id, "Pinned iSCSI session is already absent");
            return Ok(());
        };
        if active.iqn != pinned.iqn || active.portal != pinned.portal {
            // Session ids can be reused after host/session recovery. Never let a
            // stale journal log out a different current session.
            return Ok(());
        }

        info!(session_id = %pinned.session_id, portal = %pinned.portal.host, "Logging out pinned iSCSI session");
        let mut command = Command::new("iscsiadm");
        command.args(["-m", "session", "-r", &pinned.session_id, "--logout"]);
        let output = run_storage_command(&mut command, "iscsiadm session logout").await?;
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let after = parse_sessions(&Self::session_output().await?)?;
        let still_active = after.iter().any(|session| {
            session.session_id == pinned.session_id
                && session.iqn == pinned.iqn
                && session.portal == pinned.portal
        });
        if still_active {
            return Err(eyre!(
                "iscsiadm logout {} failed to remove pinned session: {stderr}",
                output.status
            ));
        }
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn detach(&self) -> Result<()> {
        Err(eyre!(
            "iSCSI logout requires a durable pinned session identity; refusing to infer ownership from the URI"
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Portal {
    host: String,
    port: u16,
}

fn parse_portal(value: &str) -> Result<Portal> {
    // iscsiadm session output appends the session id after a comma, while the
    // requested endpoint is stored as host:port.
    let value = value.split_once(',').map_or(value, |(portal, _)| portal);
    let (host, port) = if let Some(bracketed) = value.strip_prefix('[') {
        let (host, suffix) = bracketed
            .split_once(']')
            .ok_or_else(|| eyre!("Malformed bracketed iSCSI portal: {value}"))?;
        let port = suffix
            .strip_prefix(':')
            .ok_or_else(|| eyre!("Missing port in iSCSI portal: {value}"))?;
        (host, port)
    } else {
        value
            .rsplit_once(':')
            .ok_or_else(|| eyre!("Missing port in iSCSI portal: {value}"))?
    };
    if host.is_empty() {
        return Err(eyre!("Missing host in iSCSI portal: {value}"));
    }
    let port = port
        .parse::<u16>()
        .map_err(|error| eyre!("Invalid port in iSCSI portal {value}: {error}"))?;
    Ok(Portal {
        host: host.to_owned(),
        port,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ISCSISession {
    session_id: String,
    portal: Portal,
    iqn: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PinnedISCSISession {
    session_id: String,
    portal: Portal,
    iqn: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingISCSISession {
    iqn: String,
    requested_portal: Portal,
}

enum ISCSIAttachment {
    Pending(PendingISCSISession),
    Pinned(PinnedISCSISession),
}

fn parse_attachment(value: &str) -> Result<ISCSIAttachment> {
    if let Some(pending) = value.strip_prefix("pending:") {
        return serde_json::from_str(pending)
            .map(ISCSIAttachment::Pending)
            .wrap_err("Invalid persisted pending iSCSI login identity");
    }
    serde_json::from_str(value)
        .map(ISCSIAttachment::Pinned)
        .wrap_err("Invalid persisted iSCSI session identity")
}

fn parse_sessions(session_output: &str) -> Result<Vec<ISCSISession>> {
    let mut sessions = Vec::new();
    for line in session_output.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.is_empty() {
            continue;
        }
        if fields.first() != Some(&"tcp:") || fields.len() < 4 {
            return Err(eyre!("Malformed iSCSI session row: {line}"));
        }
        let target_name = fields[3];
        if !["iqn.", "eui.", "naa."]
            .iter()
            .any(|prefix| target_name.starts_with(prefix))
        {
            return Err(eyre!(
                "Unrecognized iSCSI target identifier in session row: {line}"
            ));
        }
        let session_id = fields[1]
            .strip_prefix('[')
            .and_then(|field| field.strip_suffix(']'))
            .filter(|field| !field.is_empty())
            .ok_or_else(|| eyre!("Malformed iSCSI session id in row: {line}"))?
            .to_owned();
        let portal = parse_portal(fields[2])?;
        sessions.push(ISCSISession {
            session_id,
            portal,
            iqn: target_name.to_owned(),
        });
    }
    Ok(sessions)
}

fn newly_acquired_sessions(
    session_output: &str,
    before_ids: &BTreeSet<String>,
    iqn: &str,
    requested_portal: &Portal,
    requested_ips: &BTreeSet<IpAddr>,
) -> Result<Vec<ISCSISession>> {
    Ok(parse_sessions(session_output)?
        .into_iter()
        .filter(|session| {
            session.iqn == iqn
                && !before_ids.contains(&session.session_id)
                && session.portal.port == requested_portal.port
                && session
                    .portal
                    .host
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| requested_ips.contains(&ip))
        })
        .collect())
}

fn session_portals_for_iqn(session_output: &str, iqn: &str) -> Result<Vec<Portal>> {
    Ok(parse_sessions(session_output)?
        .into_iter()
        .filter(|session| session.iqn == iqn)
        .map(|session| session.portal)
        .collect())
}

fn normalized_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn endpoint_equivalent_with_ips(
    left: &Portal,
    right: &Portal,
    left_ips: &[IpAddr],
    right_ips: &[IpAddr],
) -> bool {
    left.port == right.port
        && (normalized_host(&left.host) == normalized_host(&right.host)
            || left_ips.iter().any(|ip| right_ips.contains(ip)))
}

async fn portal_ip_addresses(
    portal: &Portal,
    cache: &mut BTreeMap<String, BTreeSet<IpAddr>>,
    resolver: &dyn PortalResolver,
) -> Result<BTreeSet<IpAddr>> {
    if let Ok(ip) = portal.host.parse::<IpAddr>() {
        return Ok(std::iter::once(ip).collect());
    }
    let cache_key = normalized_host(&portal.host);
    if let Some(addresses) = cache.get(&cache_key) {
        return Ok(addresses.clone());
    }
    let addresses = resolver.resolve(&portal.host, portal.port).await?;
    cache.insert(cache_key, addresses.clone());
    Ok(addresses)
}

async fn portals_equivalent(
    left: &Portal,
    right: &Portal,
    cache: &mut BTreeMap<String, BTreeSet<IpAddr>>,
    resolver: &dyn PortalResolver,
) -> Result<bool> {
    if left.port != right.port {
        return Ok(false);
    }
    if normalized_host(&left.host) == normalized_host(&right.host) {
        return Ok(true);
    }
    let left_is_ip = left.host.parse::<IpAddr>().is_ok();
    let right_is_ip = right.host.parse::<IpAddr>().is_ok();
    if left_is_ip && right_is_ip {
        return Ok(false);
    }

    // DNS can positively establish an alias, but a current non-match cannot
    // prove a historical hostname session is absent (the record may have
    // changed since login). Preserve uncertainty in that case.
    let left_ips = portal_ip_addresses(left, cache, resolver).await?;
    let right_ips = portal_ip_addresses(right, cache, resolver).await?;
    if endpoint_equivalent_with_ips(
        left,
        right,
        &left_ips.into_iter().collect::<Vec<_>>(),
        &right_ips.into_iter().collect::<Vec<_>>(),
    ) {
        Ok(true)
    } else {
        Err(eyre!(
            "Cannot prove whether historical iSCSI portal {} matches current portal {}",
            right.host,
            left.host
        ))
    }
}

async fn session_matches_target_inner(
    session_output: &str,
    target: &ISCSITarget,
    resolver: &dyn PortalResolver,
) -> Result<bool> {
    let target_portal = parse_portal(&target.host)?;
    let portals = session_portals_for_iqn(session_output, &target.iqn)?;
    let mut cache = BTreeMap::new();
    let mut verification_error = None;
    for portal in portals {
        match portals_equivalent(&target_portal, &portal, &mut cache, resolver).await {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) => verification_error = Some(error),
        }
    }
    if let Some(error) = verification_error {
        return Err(error);
    }
    Ok(false)
}

async fn session_matches_target_with_timeout(
    session_output: &str,
    target: &ISCSITarget,
    timeout: Duration,
    resolver: &dyn PortalResolver,
) -> Result<bool> {
    tokio::time::timeout(
        timeout,
        session_matches_target_inner(session_output, target, resolver),
    )
    .await
    .map_err(|_| eyre!("Timed out verifying iSCSI portal identity after {timeout:?}"))?
}

#[cfg(test)]
async fn session_matches_target(session_output: &str, target: &ISCSITarget) -> Result<bool> {
    session_matches_target_with_timeout(
        session_output,
        target,
        SESSION_VERIFICATION_TIMEOUT,
        &SystemPortalResolver,
    )
    .await
}

#[cfg(test)]
fn logout_failure_result(stderr: &str, active_session: Result<bool>) -> Result<()> {
    match active_session {
        Ok(true) => Err(eyre!("iscsiadm logout failed: {stderr}")),
        Ok(false) => Ok(()),
        Err(check_error) => Err(eyre!(
            "iscsiadm logout failed: {stderr}; could not verify session state: {check_error:#}"
        )),
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

pub struct ISCSIStorage;

impl ISCSIStorage {
    async fn resolve_claimed_with_resolver(
        &self,
        uri: &Url,
        is_new: bool,
        existing_attachment: Option<&str>,
        persist_attachment: &mut (dyn for<'a> FnMut(&'a str) -> Result<()> + Send),
        resolver: &dyn PortalResolver,
        verification_timeout: Duration,
    ) -> Result<PathBuf> {
        let target = ISCSITarget::from(uri);
        if is_new {
            target.attach_with_identity(persist_attachment).await
        } else {
            let attachment = existing_attachment.ok_or_else(|| {
                eyre!("Existing iSCSI ownership claim has no pinned session identity")
            })?;
            let pinned = match parse_attachment(attachment)? {
                ISCSIAttachment::Pinned(pinned) => pinned,
                ISCSIAttachment::Pending(_) => {
                    return Err(eyre!(
                        "iSCSI login submission is uncertain; refusing to retry or adopt a later session"
                    ));
                }
            };
            if pinned.iqn != target.iqn {
                return Err(eyre!("Existing iSCSI claim belongs to a different IQN"));
            }
            let sessions = parse_sessions(&ISCSITarget::session_output().await?)?;
            let Some(active) = sessions.into_iter().find(|session| {
                session.session_id == pinned.session_id
                    && session.iqn == pinned.iqn
                    && session.portal == pinned.portal
            }) else {
                return Err(eyre!("Pinned iSCSI session is no longer active"));
            };
            let output = format!(
                "tcp: [{}] {}:{},1 {}",
                active.session_id, active.portal.host, active.portal.port, active.iqn
            );
            if !session_matches_target_with_timeout(
                &output,
                &target,
                verification_timeout,
                resolver,
            )
            .await?
            {
                return Err(eyre!(
                    "Pinned iSCSI session does not match the requested portal"
                ));
            }
            Ok(target.to_device_path())
        }
    }
}

#[async_trait]
impl StorageDriver for ISCSIStorage {
    fn scheme(&self) -> &'static str {
        "iscsi"
    }

    fn ownership_key(&self, uri: &Url) -> Result<Option<String>> {
        let target = ISCSITarget::from(uri);
        let portal = parse_portal(&target.host)?;
        // Canonicalize the requested portal while keeping DNS names stable
        // across address changes; LUNs at the same portal/IQN share one claim.
        Ok(Some(format!(
            "iscsi:{}@{}:{}",
            target.iqn,
            normalized_host(&portal.host),
            portal.port
        )))
    }

    async fn ensure_exclusive(&self, uri: &Url) -> Result<()> {
        ISCSITarget::from(uri).ensure_unattached().await
    }

    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        ISCSITarget::from(uri).attach().await
    }

    async fn resolve_claimed(
        &self,
        uri: &Url,
        is_new: bool,
        existing_attachment: Option<&str>,
        persist_attachment: &mut (dyn for<'a> FnMut(&'a str) -> Result<()> + Send),
    ) -> Result<PathBuf> {
        self.resolve_claimed_with_resolver(
            uri,
            is_new,
            existing_attachment,
            persist_attachment,
            &SystemPortalResolver,
            SESSION_VERIFICATION_TIMEOUT,
        )
        .await
    }

    async fn release(&self, uri: &Url) -> Result<()> {
        ISCSITarget::from(uri).detach().await
    }

    async fn release_claimed(&self, uri: &Url, attachment: Option<&str>) -> Result<()> {
        self.release_claimed_with_intent(uri, attachment, StorageReleaseState::default())
            .await
    }

    async fn release_claimed_with_intent(
        &self,
        uri: &Url,
        attachment: Option<&str>,
        release_state: StorageReleaseState,
    ) -> Result<()> {
        let target = ISCSITarget::from(uri);
        let Some(attachment) = attachment else {
            if !release_state.claim_exists
                || release_state.recovered
                || release_state.already_started
            {
                return Err(eyre!(
                    "iSCSI cleanup has no durable submission or session identity record; retaining storage claim"
                ));
            }
            let sessions = parse_sessions(&ISCSITarget::session_output().await?)?;
            if sessions.iter().any(|session| session.iqn == target.iqn) {
                return Err(eyre!(
                    "Refusing to release iSCSI target {} without pinned session identity",
                    target.iqn
                ));
            }
            // A pre-submission claim may have no session, but a submitted login
            // is represented by a durable Pending attachment, never None.
            return Ok(());
        };
        match parse_attachment(attachment)? {
            ISCSIAttachment::Pending(pending) => Err(eyre!(
                "iSCSI login for {} at {}:{} is unresolved; retaining storage claim",
                pending.iqn,
                pending.requested_portal.host,
                pending.requested_portal.port
            )),
            ISCSIAttachment::Pinned(pinned)
                if release_state.already_started || release_state.recovered =>
            {
                let sessions = parse_sessions(&ISCSITarget::session_output().await?)?;
                let still_same = sessions.iter().any(|session| {
                    session.session_id == pinned.session_id
                        && session.iqn == pinned.iqn
                        && session.portal == pinned.portal
                });
                if still_same {
                    Err(eyre!(
                        "iSCSI release is uncertain for recovered or previously submitted session {}; refusing a second logout",
                        pinned.session_id
                    ))
                } else {
                    // An absent or changed tuple is not a target for logout.
                    // The old daemon/session is no longer represented, so no
                    // destructive command is necessary.
                    Ok(())
                }
            }
            ISCSIAttachment::Pinned(_) => target.detach_pinned(attachment).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::StorageDriverTransformer;
    use super::*;
    use crate::ch_driver::transform::{ConfigTransform, StorageCleanupContext};
    use cloud_hypervisor_client::models::VmConfig;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn delayed_daemon_login_keeps_pending_claim_after_cli_timeout_and_restart() {
        let root = std::env::temp_dir().join(format!(
            "odorobo-fake-iscsi-delayed-{}",
            ulid::Ulid::generate()
        ));
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create fake iscsiadm directory");
        let cli = bin_dir.join("iscsiadm");
        std::fs::write(
            &cli,
            r#"#!/bin/sh
state="$ODOROBO_FAKE_ISCSI_STATE"
if [ "$1" = "-m" ] && [ "$2" = "session" ]; then
    if [ -s "$state" ]; then
        cat "$state"
        exit 0
    fi
    echo 'iscsiadm: No active sessions.' >&2
    exit 1
fi
case " $* " in
    *" --login "*)
        (sleep 1.5; printf 'tcp: [9] 192.0.2.10:3260,1 iqn.2026-03.example:delayed\n' > "$state") >/dev/null 2>&1 </dev/null &
        exec sleep 10
        ;;
esac
echo 'unexpected iscsiadm command' >&2
exit 2
"#,
        )
        .expect("write fake iscsiadm CLI");
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755))
            .expect("make fake iscsiadm executable");
        let state = root.join("sessions");
        let claim = root.join("claim");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ch_driver::transform::storage::iscsi::tests::fake_iscsi_delayed_login_child",
                "--nocapture",
            ])
            .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
            .env("ODOROBO_FAKE_ISCSI_CHILD", "1")
            .env("ODOROBO_FAKE_ISCSI_STATE", &state)
            .env("ODOROBO_FAKE_ISCSI_CLAIM", &claim)
            .output()
            .expect("run delayed iSCSI daemon regression child");
        assert!(
            output.status.success(),
            "delayed iSCSI child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(root).expect("remove fake iSCSI fixture");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fake_iscsi_delayed_login_child() {
        if std::env::var_os("ODOROBO_FAKE_ISCSI_CHILD").is_none() {
            return;
        }
        let claim_path = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_ISCSI_CLAIM").expect("fake ownership claim"),
        );
        let target = ISCSITarget {
            host: "192.0.2.10:3260".to_owned(),
            iqn: "iqn.2026-03.example:delayed".to_owned(),
            lun: 0,
        };
        let mut persist = |attachment: &str| {
            std::fs::write(&claim_path, attachment)
                .map_err(|error| eyre!("persist fake iSCSI claim: {error}"))
        };
        let error = target
            .attach_with_identity_and_timeout(&mut persist, Duration::from_millis(40))
            .await
            .expect_err("iscsiadm CLI should time out after submission");
        assert!(error.to_string().contains("Timed out"), "{error:#}");
        let pending = std::fs::read_to_string(&claim_path).expect("pending claim is durable");
        assert!(pending.starts_with("pending:"), "{pending}");
        let before_completion = ISCSITarget::session_output()
            .await
            .expect("empty query before daemon completion");
        assert!(before_completion.is_empty(), "{before_completion}");

        // A restarted cleanup sees the durable pending phase while the session
        // listing is still empty and must retain the claim without inferring
        // that the daemon cancelled the submitted login.
        let uri = Url::parse("iscsi://192.0.2.10:3260/iqn.2026-03.example:delayed/0").unwrap();
        let mut attachment = |_: &Url, _: &str| {
            Ok(Some(
                std::fs::read_to_string(&claim_path).expect("read pending claim"),
            ))
        };
        let mut begin_release = |_: &Url, _: &str| {
            Ok(StorageReleaseState {
                recovered: true,
                ..Default::default()
            })
        };
        let mut forgotten = false;
        let mut forget = |_: &Url, _: &str| {
            forgotten = true;
            Ok(())
        };
        let mut cleanup = StorageCleanupContext {
            attachment: &mut attachment,
            begin_release: &mut begin_release,
            forget: &mut forget,
        };
        let transformer = StorageDriverTransformer::default();
        let error = transformer
            .teardown_with_storage_ownership(
                "delayed-login",
                &mut VmConfig::default(),
                std::slice::from_ref(&uri),
                &mut cleanup,
            )
            .expect_err("pending daemon login remains unresolved");
        assert!(error.to_string().contains("unresolved"), "{error:#}");
        assert!(!forgotten, "uncertain claim must remain durable");

        // The independent fake daemon completes after iscsiadm was killed and
        // the empty listing was already observed by restart cleanup.
        tokio::time::sleep(Duration::from_millis(1_600)).await;
        let sessions = ISCSITarget::session_output()
            .await
            .expect("query delayed session");
        assert!(sessions.contains("[9] 192.0.2.10:3260"), "{sessions}");
    }

    struct NeverResolver;

    impl PortalResolver for NeverResolver {
        fn resolve<'a>(&'a self, _host: &'a str, _port: u16) -> ResolverFuture<'a> {
            Box::pin(std::future::pending())
        }
    }

    #[test]
    fn release_intent_prevents_second_logout_of_same_tuple_replacement() {
        let root = std::env::temp_dir().join(format!(
            "odorobo-fake-iscsi-retry-{}",
            ulid::Ulid::generate()
        ));
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create fake iscsiadm directory");
        let cli = bin_dir.join("iscsiadm");
        std::fs::write(
            &cli,
            r#"#!/bin/sh
state="$ODOROBO_FAKE_ISCSI_STATE"
queries="$ODOROBO_FAKE_ISCSI_QUERIES"
log="$ODOROBO_FAKE_ISCSI_LOG"
if [ "$1" = "-m" ] && [ "$2" = "session" ] && [ "$3" = "-r" ]; then
    printf 'LOGOUT\n' >> "$log"
    : > "$state"
    exit 0
fi
if [ "$1" = "-m" ] && [ "$2" = "session" ]; then
    count=0
    [ -f "$queries" ] && count=$(cat "$queries")
    count=$((count + 1))
    printf '%s' "$count" > "$queries"
    if [ "$count" = "2" ]; then
        echo 'simulated query failure after successful logout' >&2
        exit 2
    fi
    if [ -s "$state" ]; then
        cat "$state"
        exit 0
    fi
    echo 'iscsiadm: No active sessions.' >&2
    exit 1
fi
case " $* " in
    *" --logout "*)
        printf 'LOGOUT\n' >> "$log"
        : > "$state"
        exit 0
        ;;
esac
echo 'unexpected iscsiadm command' >&2
exit 2
"#,
        )
        .expect("write fake iscsiadm CLI");
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755))
            .expect("make fake iscsiadm executable");
        let state = root.join("sessions");
        let queries = root.join("queries");
        let log = root.join("log");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ch_driver::transform::storage::iscsi::tests::fake_iscsi_release_retry_child",
                "--nocapture",
            ])
            .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
            .env("ODOROBO_FAKE_ISCSI_RETRY_CHILD", "1")
            .env("ODOROBO_FAKE_ISCSI_STATE", &state)
            .env("ODOROBO_FAKE_ISCSI_QUERIES", &queries)
            .env("ODOROBO_FAKE_ISCSI_LOG", &log)
            .output()
            .expect("run fake iSCSI release retry regression child");
        assert!(
            output.status.success(),
            "fake iSCSI release retry child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(root).expect("remove fake iSCSI fixture");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fake_iscsi_release_retry_child() {
        if std::env::var_os("ODOROBO_FAKE_ISCSI_RETRY_CHILD").is_none() {
            return;
        }
        let state = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_ISCSI_STATE").expect("fake session state"),
        );
        let log = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_ISCSI_LOG").expect("fake command log"),
        );
        let original = "tcp: [4] 192.0.2.10:3260,1 iqn.2026-03.example:retry\n";
        std::fs::write(&state, original).expect("initialize original session");
        std::fs::write(&log, "").expect("initialize command log");
        let uri = Url::parse("iscsi://192.0.2.10:3260/iqn.2026-03.example:retry/0").unwrap();
        let pinned = serde_json::to_string(&PinnedISCSISession {
            session_id: "4".to_owned(),
            portal: Portal {
                host: "192.0.2.10".to_owned(),
                port: 3260,
            },
            iqn: "iqn.2026-03.example:retry".to_owned(),
        })
        .unwrap();
        let release_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let forget_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transformer = StorageDriverTransformer::default();
        let attachment_value = pinned;
        let mut attachment = move |_: &Url, _: &str| Ok(Some(attachment_value.clone()));
        let started_for_callback = std::sync::Arc::clone(&release_started);
        let mut begin_release = move |_: &Url, _: &str| {
            let already_started =
                started_for_callback.swap(true, std::sync::atomic::Ordering::SeqCst);
            Ok(StorageReleaseState {
                already_started,
                recovered: false,
                claim_exists: true,
            })
        };
        let forgotten_for_callback = std::sync::Arc::clone(&forget_called);
        let mut forget = move |_: &Url, _: &str| {
            forgotten_for_callback.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };
        let mut cleanup = StorageCleanupContext {
            attachment: &mut attachment,
            begin_release: &mut begin_release,
            forget: &mut forget,
        };
        let mut config = VmConfig::default();
        let first_error = transformer
            .teardown_with_storage_ownership(
                "iscsi-retry",
                &mut config,
                std::slice::from_ref(&uri),
                &mut cleanup,
            )
            .expect_err("post-logout verification fails");
        assert!(
            first_error
                .to_string()
                .contains("Failed to query iSCSI sessions"),
            "{first_error:#}"
        );
        assert!(!forget_called.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "LOGOUT\n");

        // An external initiator reconnects and reuses the same numeric session
        // ID, portal, and IQN before restart cleanup. Prior release intent must
        // prevent a second destructive logout despite the identical tuple.
        std::fs::write(&state, original).expect("simulate replacement session");
        let second_error = transformer
            .teardown_with_storage_ownership(
                "iscsi-retry",
                &mut config,
                std::slice::from_ref(&uri),
                &mut cleanup,
            )
            .expect_err("release intent makes identical tuple uncertain");
        assert!(second_error.to_string().contains("release is uncertain"));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "LOGOUT\n");
        assert!(!forget_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn shared_existing_claim_resolution_bounds_hostname_verification_and_retains_journal() {
        let root = std::env::temp_dir().join(format!(
            "odorobo-fake-iscsi-shared-resolve-{}",
            ulid::Ulid::generate()
        ));
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create fake iscsiadm directory");
        let cli = bin_dir.join("iscsiadm");
        std::fs::write(
            &cli,
            r#"#!/bin/sh
if [ "$1" = "-m" ] && [ "$2" = "session" ]; then
    printf 'tcp: [4] 192.0.2.10:3260,1 iqn.2026-03.example:shared\n'
    exit 0
fi
echo 'unexpected iscsiadm command' >&2
exit 2
"#,
        )
        .expect("write fake iscsiadm CLI");
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755))
            .expect("make fake iscsiadm executable");
        let claim = root.join("claim");
        let journal = root.join("journal");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ch_driver::transform::storage::iscsi::tests::fake_iscsi_shared_resolve_child",
                "--nocapture",
            ])
            .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
            .env("ODOROBO_FAKE_ISCSI_SHARED_CHILD", "1")
            .env("ODOROBO_FAKE_ISCSI_SHARED_CLAIM", &claim)
            .env("ODOROBO_FAKE_ISCSI_SHARED_JOURNAL", &journal)
            .output()
            .expect("run bounded shared iSCSI resolution child");
        assert!(
            output.status.success(),
            "shared iSCSI resolve child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(root).expect("remove fake iSCSI fixture");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fake_iscsi_shared_resolve_child() {
        if std::env::var_os("ODOROBO_FAKE_ISCSI_SHARED_CHILD").is_none() {
            return;
        }
        let claim_path = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_ISCSI_SHARED_CLAIM").expect("shared claim path"),
        );
        let journal_path = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_ISCSI_SHARED_JOURNAL").expect("shared journal path"),
        );
        let uri =
            Url::parse("iscsi://target.example.com:3260/iqn.2026-03.example:shared/0").unwrap();
        let pinned = serde_json::to_string(&PinnedISCSISession {
            session_id: "4".to_owned(),
            portal: Portal {
                host: "192.0.2.10".to_owned(),
                port: 3260,
            },
            iqn: "iqn.2026-03.example:shared".to_owned(),
        })
        .unwrap();
        std::fs::write(&claim_path, &pinned).expect("persist existing shared claim");
        std::fs::write(&journal_path, uri.as_str()).expect("persist existing VM journal");

        let persist_called = std::sync::atomic::AtomicBool::new(false);
        let mut persist = |attachment: &str| {
            persist_called.store(true, std::sync::atomic::Ordering::SeqCst);
            std::fs::write(&claim_path, attachment)
                .map_err(|error| eyre!("persist shared test claim: {error}"))
        };
        let started = tokio::time::Instant::now();
        let error = ISCSIStorage
            .resolve_claimed_with_resolver(
                &uri,
                false,
                Some(&pinned),
                &mut persist,
                &NeverResolver,
                Duration::from_millis(20),
            )
            .await
            .expect_err("hostname verification of a pinned numeric session must time out");

        assert!(error.to_string().contains("Timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            !persist_called.load(std::sync::atomic::Ordering::SeqCst),
            "shared verification must not rewrite its claim"
        );
        assert_eq!(std::fs::read_to_string(&claim_path).unwrap(), pinned);
        assert_eq!(
            std::fs::read_to_string(&journal_path).unwrap(),
            uri.as_str()
        );
    }

    #[tokio::test]
    async fn never_ending_portal_resolution_obeys_total_verification_deadline() {
        let target = ISCSITarget {
            host: "target.example.com:3260".to_owned(),
            iqn: "iqn.2026-03.example:disk".to_owned(),
            lun: 0,
        };
        let started = tokio::time::Instant::now();
        let error = session_matches_target_with_timeout(
            "tcp: [4] 192.0.2.10:3260,1 iqn.2026-03.example:disk",
            &target,
            Duration::from_millis(20),
            &NeverResolver,
        )
        .await
        .expect_err("resolver timeout must not assert target absence");
        assert!(error.to_string().contains("Timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    struct StaticResolver(BTreeMap<String, BTreeSet<IpAddr>>);

    impl PortalResolver for StaticResolver {
        fn resolve<'a>(&'a self, host: &'a str, _port: u16) -> ResolverFuture<'a> {
            Box::pin(async move {
                self.0
                    .get(host)
                    .cloned()
                    .ok_or_else(|| eyre!("no test DNS record for {host}"))
            })
        }
    }

    #[tokio::test]
    async fn changed_dns_does_not_prove_historical_session_absent() {
        let target = ISCSITarget {
            host: "target.example.com:3260".to_owned(),
            iqn: "iqn.2026-03.example:disk".to_owned(),
            lun: 0,
        };
        let resolver = StaticResolver(BTreeMap::from([(
            "target.example.com".to_owned(),
            BTreeSet::from(["192.0.2.11".parse().unwrap()]),
        )]));
        let session_result = session_matches_target_with_timeout(
            "tcp: [4] 192.0.2.10:3260,1 iqn.2026-03.example:disk",
            &target,
            SESSION_VERIFICATION_TIMEOUT,
            &resolver,
        )
        .await;
        assert!(
            session_result.is_err(),
            "current DNS resolving to B cannot establish that an A session is absent"
        );
        assert!(logout_failure_result("logout failed", session_result).is_err());
    }

    #[tokio::test]
    async fn session_match_requires_exact_portal_and_iqn() {
        let target = ISCSITarget {
            host: "192.168.1.10:3260".to_owned(),
            iqn: "iqn.2026-03.example:disk".to_owned(),
            lun: 0,
        };
        assert!(
            session_matches_target(
                "tcp: [4] 192.168.1.10:3260,1 iqn.2026-03.example:disk",
                &target
            )
            .await
            .unwrap()
        );
        assert!(
            !session_matches_target(
                "tcp: [4] 192.168.1.10:3260,1 iqn.2026-03.example:disk-extra",
                &target
            )
            .await
            .unwrap()
        );
        assert!(
            !session_matches_target(
                "tcp: [4] 192.168.1.10:13260,1 iqn.2026-03.example:disk",
                &target
            )
            .await
            .unwrap()
        );
        assert!(
            !session_matches_target(
                "tcp: [4] 192.168.1.100:3260,1 iqn.2026-03.example:disk",
                &target
            )
            .await
            .unwrap()
        );
    }

    #[test]
    fn hostname_and_numeric_portals_match_only_after_ip_equivalence() {
        let hostname = Portal {
            host: "target.example.com".to_owned(),
            port: 3260,
        };
        let numeric = Portal {
            host: "192.0.2.10".to_owned(),
            port: 3260,
        };
        let resolved = "192.0.2.10".parse::<IpAddr>().unwrap();
        assert!(endpoint_equivalent_with_ips(
            &hostname,
            &numeric,
            &[resolved],
            &[resolved]
        ));
        assert!(!endpoint_equivalent_with_ips(
            &hostname,
            &Portal {
                port: 3261,
                ..numeric
            },
            &[resolved],
            &[resolved]
        ));
    }

    #[test]
    fn failed_logout_with_hostname_uri_and_numeric_active_portal_retains_error() {
        let target = ISCSITarget {
            host: "target.example.com:3260".to_owned(),
            iqn: "iqn.2026-03.example:disk".to_owned(),
            lun: 0,
        };
        let session_output = "tcp: [4] 192.0.2.10:3260,1 iqn.2026-03.example:disk";
        let target_portal = parse_portal(&target.host).unwrap();
        let session_portal = session_portals_for_iqn(session_output, &target.iqn)
            .unwrap()
            .pop()
            .unwrap();
        let resolved_target_ip = "192.0.2.10".parse::<IpAddr>().unwrap();
        let session_ip = session_portal.host.parse::<IpAddr>().unwrap();
        assert!(endpoint_equivalent_with_ips(
            &target_portal,
            &session_portal,
            &[resolved_target_ip],
            &[session_ip]
        ));

        let error = logout_failure_result("target still logged in", Ok(true))
            .expect_err("active matching session must retain logout failure");
        assert!(error.to_string().contains("target still logged in"));
    }

    #[test]
    fn failed_logout_with_uncertain_session_state_retains_original_error() {
        let error = logout_failure_result("logout command failed", Err(eyre!("DNS failed")))
            .expect_err("uncertain endpoint identity must not be reported as success");
        let message = error.to_string();
        assert!(message.contains("logout command failed"));
        assert!(message.contains("DNS failed"));
    }

    #[test]
    fn login_result_is_pinned_only_to_the_requested_numeric_portal() {
        let iqn = "iqn.2026-03.example:shared";
        let portal_a = Portal {
            host: "192.0.2.10".to_owned(),
            port: 3260,
        };
        let addresses = BTreeSet::from(["192.0.2.10".parse().unwrap()]);
        let concurrent = newly_acquired_sessions(
            "tcp: [4] 192.0.2.10:3260,1 iqn.2026-03.example:shared\ntcp: [5] 192.0.2.11:3260,1 iqn.2026-03.example:shared",
            &BTreeSet::new(),
            iqn,
            &portal_a,
            &addresses,
        )
        .unwrap();
        assert_eq!(concurrent.len(), 1);
        assert_eq!(concurrent[0].session_id, "4");
        assert_eq!(concurrent[0].portal.host, "192.0.2.10");

        let only_foreign_portal = newly_acquired_sessions(
            "tcp: [5] 192.0.2.11:3260,1 iqn.2026-03.example:shared",
            &BTreeSet::new(),
            iqn,
            &portal_a,
            &addresses,
        )
        .unwrap();
        assert!(only_foreign_portal.is_empty());
    }

    #[test]
    fn session_parser_recognizes_iqn_eui_and_naa_target_names() {
        let sessions = parse_sessions(
            "tcp: [4] 192.0.2.10:3260,1 iqn.2026-03.example:disk
 tcp: [5] 192.0.2.11:3260,1 eui.0011223344556677
 tcp: [6] 192.0.2.12:3260,1 naa.6001405abc123456",
        )
        .expect("all supported iSCSI name formats should be parsed");
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[1].iqn, "eui.0011223344556677");
        assert_eq!(sessions[2].iqn, "naa.6001405abc123456");
    }

    #[test]
    fn unrecognized_session_target_row_is_uncertain_not_absent() {
        let error = parse_sessions("tcp: [4] 192.0.2.10:3260,1 naa-target-without-prefix")
            .expect_err("unrecognized target formats must not be treated as absent");
        assert!(
            error
                .to_string()
                .contains("Unrecognized iSCSI target identifier")
        );
    }

    #[test]
    fn malformed_session_for_exact_iqn_is_uncertain() {
        let _error = session_portals_for_iqn(
            "tcp: [4] invalid-portal iqn.2026-03.example:disk",
            "iqn.2026-03.example:disk",
        )
        .unwrap_err();
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
