use super::{StorageDriver, run_storage_command};
use crate::ch_driver::transform::StorageReleaseState;
use async_trait::async_trait;
use serde::Deserialize;
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
    let mut command = Command::new("rbd");
    command
        .args(rbd_extra_args())
        .args(["device", "list", "--format", "json"]);
    let output = run_storage_command(&mut command, "rbd device list").await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("rbd command failed: {stderr}"));
    }

    let output_str = String::from_utf8_lossy(&output.stdout);
    rbd_lines_list(&output_str)
}

#[derive(Debug, Deserialize)]
struct RbdDeviceMapping {
    pool: String,
    namespace: String,
    #[serde(alias = "image")]
    name: String,
    snap: String,
    device: String,
}

#[tracing::instrument]
fn rbd_lines_list(input: &str) -> Result<Vec<(String, String)>> {
    // Text output is ambiguous when namespace is empty and cannot safely
    // distinguish images from snapshots. Require the complete structured record
    // so an unrecognized response is an error rather than false absence.
    let mappings: Vec<RbdDeviceMapping> = serde_json::from_str(input)
        .map_err(|error| eyre!("Failed to parse rbd device list JSON: {error}"))?;
    let mappings = mappings
        .into_iter()
        .map(|mapping| {
            let mut path = format!("{}/", mapping.pool);
            if !mapping.namespace.is_empty() {
                path.push_str(&mapping.namespace);
                path.push('/');
            }
            path.push_str(&mapping.name);
            if mapping.snap != "-" {
                path.push('@');
                path.push_str(&mapping.snap);
            }
            (path, mapping.device)
        })
        .collect::<Vec<_>>();

    trace!(?mappings, "Parsed RBD device list");
    Ok(mappings)
}

fn rbd_mapping_exists(mappings: &[(String, String)], rbd_path: &str) -> bool {
    mappings.iter().any(|(path, _)| path == rbd_path)
}

fn parse_map_device(stdout: &str) -> Result<String> {
    // Parse the path printed by `rbd device map`, but do not treat stdout as
    // ownership proof: a successful Ceph map can correspond to another mapper's
    // udev event. The post-map list must contain one and only one image row.
    let devices = stdout
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("/dev/rbd"))
        .collect::<Vec<_>>();
    if devices.len() != 1 || devices[0].len() <= "/dev/rbd".len() {
        return Err(eyre!(
            "rbd map did not report exactly one kernel device path: {stdout:?}"
        ));
    }
    Ok(devices[0].to_owned())
}

fn verify_exact_mapping(
    mappings: &[(String, String)],
    rbd_path: &str,
    expected_device: &str,
) -> Result<()> {
    let image_rows = mappings
        .iter()
        .filter(|(path, _)| path == rbd_path)
        .collect::<Vec<_>>();
    if image_rows.len() != 1 || image_rows[0].1 != expected_device {
        return Err(eyre!(
            "Expected exactly one RBD mapping row for {rbd_path} on expected device {expected_device}; found {} matching image rows",
            image_rows.len()
        ));
    }
    if mappings
        .iter()
        .any(|(path, listed_device)| listed_device == expected_device && path != rbd_path)
    {
        return Err(eyre!(
            "RBD device {expected_device} is also listed for a different image; identity is ambiguous"
        ));
    }
    Ok(())
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

    /// Rejects an existing mapping rather than reusing a resource this VM
    /// cannot safely claim ownership of.
    async fn ensure_unmapped(&self) -> Result<()> {
        let rbd_path = self.rbd_path();
        let mappings = rbd_map_list().await?;
        if rbd_mapping_exists(&mappings, &rbd_path) {
            return Err(eyre!(
                "RBD image {rbd_path} is already mapped; cross-VM mapping sharing is unsupported"
            ));
        }
        Ok(())
    }

    /// Maps the RBD image to a kernel block device, rejecting existing mappings.
    #[tracing::instrument(skip(self))]
    pub async fn map(&self) -> Result<PathBuf> {
        self.ensure_unmapped().await?;
        Ok(PathBuf::from(self.map_claimed().await?))
    }

    /// Map after the transformer's durable node-local ownership claim and
    /// exclusive precheck. Do not repeat a rejection check after journaling:
    /// such a rejection could turn a foreign mapping into our cleanup target.
    async fn map_claimed(&self) -> Result<String> {
        let rbd_path = self.rbd_path();
        info!(?rbd_path, "Mapping RBD image to device");
        let mut command = Command::new("rbd");
        command
            .args(rbd_extra_args())
            .args(["device", "map", "--options", "noudev"])
            .arg(&rbd_path);
        let output = run_storage_command(&mut command, "rbd device map").await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("rbd map failed: {stderr}"));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_map_device(&stdout)
    }

    /// Unmaps the RBD image from the kernel block device.
    #[tracing::instrument(skip(self))]
    pub async fn unmap(&self) -> Result<()> {
        let rbd_path = self.rbd_path();
        let mappings = rbd_map_list().await?;
        if !rbd_mapping_exists(&mappings, &rbd_path) {
            info!(?rbd_path, "RBD image is already unmapped");
            return Ok(());
        }
        self.unmap_argument(&rbd_path, &rbd_path, None).await
    }

    async fn unmap_claimed(&self, expected_device: &str) -> Result<()> {
        let rbd_path = self.rbd_path();
        let mappings = rbd_map_list().await?;
        let exact = mappings
            .iter()
            .filter(|(path, device)| path == &rbd_path && device == expected_device)
            .count();
        if exact == 0 {
            info!(
                ?rbd_path,
                expected_device,
                "Pinned RBD mapping is absent or replaced; leaving current mappings untouched"
            );
            return Ok(());
        }
        if exact != 1 {
            return Err(eyre!(
                "RBD image {rbd_path} has multiple rows for pinned device {expected_device}"
            ));
        }
        self.unmap_argument(&rbd_path, expected_device, Some(expected_device))
            .await
    }

    async fn unmap_argument(
        &self,
        rbd_path: &str,
        argument: &str,
        expected_device: Option<&str>,
    ) -> Result<()> {
        info!(?rbd_path, argument, "Unmapping RBD image");
        let mut command = Command::new("rbd");
        command
            .args(rbd_extra_args())
            .args(["device", "unmap", "--options", "noudev"])
            .arg(argument);
        let output = run_storage_command(&mut command, "rbd device unmap").await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return match rbd_map_list().await {
                Ok(mappings)
                    if expected_device.map_or_else(
                        || !rbd_mapping_exists(&mappings, rbd_path),
                        |expected| !mappings.iter().any(|(_, device)| device == expected),
                    ) =>
                {
                    info!(?rbd_path, "Pinned RBD mapping was unmapped concurrently");
                    Ok(())
                }
                Ok(_) => Err(eyre!("rbd unmap failed: {stderr}")),
                Err(check_error) => Err(eyre!(
                    "rbd unmap failed: {stderr}; could not verify mapping state: {check_error:#}"
                )),
            };
        }
        let mappings = rbd_map_list().await?;
        let still_mapped = expected_device.map_or_else(
            || rbd_mapping_exists(&mappings, rbd_path),
            |expected| mappings.iter().any(|(_, device)| device == expected),
        );
        if still_mapped {
            return Err(eyre!(
                "rbd unmap reported success but mapping identity remains present"
            ));
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

pub struct RbdStorage;

#[async_trait]
impl StorageDriver for RbdStorage {
    fn scheme(&self) -> &'static str {
        "rbd"
    }

    fn ownership_key(&self, uri: &Url) -> Result<Option<String>> {
        Ok(Some(format!("rbd:{}", RbdImage::try_from(uri)?.rbd_path())))
    }

    async fn ensure_exclusive(&self, uri: &Url) -> Result<()> {
        RbdImage::try_from(uri)?.ensure_unmapped().await
    }

    async fn resolve(&self, uri: &Url) -> Result<PathBuf> {
        let image = RbdImage::try_from(uri)?;
        image.map().await
    }

    async fn resolve_claimed(
        &self,
        uri: &Url,
        is_new: bool,
        existing_attachment: Option<&str>,
        persist_attachment: &mut (dyn for<'a> FnMut(&'a str) -> Result<()> + Send),
    ) -> Result<PathBuf> {
        let image = RbdImage::try_from(uri)?;
        let rbd_path = image.rbd_path();
        let mut mappings = if is_new {
            Vec::new()
        } else {
            rbd_map_list().await?
        };
        let device = if is_new {
            // Ceph may permit duplicate maps, and its output does not prove
            // which matching udev event this request caused. The post-map list
            // must identify a unique row before any device identity is pinned.
            let reported_device = image.map_claimed().await?;
            mappings = rbd_map_list().await?;
            verify_exact_mapping(&mappings, &rbd_path, &reported_device)?;
            reported_device
        } else if let Some(expected) = existing_attachment {
            if rbd_mapping_exists(&mappings, &rbd_path) {
                verify_exact_mapping(&mappings, &rbd_path, expected).map_err(|_| {
                    eyre!(
                        "RBD image {} no longer matches its pinned device {expected}",
                        rbd_path
                    )
                })?;
                expected.to_owned()
            } else {
                // A missing pinned mapping is retryable only if no other actor
                // has mapped this image in the meantime.
                let reported_device = image.map_claimed().await?;
                mappings = rbd_map_list().await?;
                verify_exact_mapping(&mappings, &rbd_path, &reported_device)?;
                reported_device
            }
        } else if rbd_mapping_exists(&mappings, &rbd_path) {
            return Err(eyre!(
                "RBD image {} is mapped but its pinned device identity was not persisted",
                rbd_path
            ));
        } else {
            // Retry a known same-VM claim whose map attempt did not take effect.
            let reported_device = image.map_claimed().await?;
            mappings = rbd_map_list().await?;
            verify_exact_mapping(&mappings, &rbd_path, &reported_device)?;
            reported_device
        };
        persist_attachment(&device)?;
        Ok(PathBuf::from(device))
    }

    async fn release(&self, uri: &Url) -> Result<()> {
        let image = RbdImage::try_from(uri)?;
        image.unmap().await
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
        let image = RbdImage::try_from(uri)?;
        let mappings = rbd_map_list().await?;
        let Some(device) = attachment else {
            if !rbd_mapping_exists(&mappings, &image.rbd_path()) {
                // A crash after journaling but before map leaves no attachment
                // to release. If anything is mapped, identity is ambiguous.
                return Ok(());
            }
            return Err(eyre!(
                "Refusing to release RBD mapping without its pinned device identity"
            ));
        };
        if release_state.already_started || release_state.recovered {
            // A prior release may have completed before a crash, or the claim
            // may come from an earlier process. `/dev/rbdN` is reusable, so if
            // the same image now occupies that path, it is impossible to
            // distinguish the original mapping from a replacement. Keep the
            // durable claim and never unmap that ambiguous identity.
            if mappings
                .iter()
                .any(|(path, current_device)| path == &image.rbd_path() && current_device == device)
            {
                return Err(eyre!(
                    "RBD release is uncertain: pinned image/device identity is still present"
                ));
            }
            return Ok(());
        }
        image.unmap_claimed(device).await
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
    fn fake_rbd_cli_interleaving_never_unmaps_a_foreign_mapping() {
        let root =
            std::env::temp_dir().join(format!("odorobo-fake-rbd-{}", ulid::Ulid::generate()));
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create fake rbd directory");
        let cli = bin_dir.join("rbd");
        std::fs::write(
            &cli,
            r#"#!/bin/sh
state="$ODOROBO_FAKE_RBD_STATE"
log="$ODOROBO_FAKE_RBD_LOG"
if [ "$1" = "device" ] && [ "$2" = "list" ]; then
    cat "$state"
    exit 0
fi
if [ "$1" = "device" ] && [ "$2" = "map" ]; then
    printf 'MAP\n' >> "$log"
    if grep -Fq '"name":"image"' "$state"; then
        echo 'already mapped' >&2
        exit 1
    fi
    printf '[{"pool":"pool","namespace":"","name":"image","snap":"-","device":"/dev/rbdB"}]' > "$state"
    exit 0
fi
if [ "$1" = "device" ] && [ "$2" = "unmap" ]; then
    printf 'UNMAP %s\n' "$5" >> "$log"
    printf '[]' > "$state"
    exit 0
fi
echo 'unexpected rbd command' >&2
exit 2
"#,
        )
        .expect("write fake rbd CLI");
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755))
            .expect("make fake rbd executable");
        let state = root.join("mappings.json");
        let log = root.join("commands.log");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ch_driver::transform::storage::rbd::tests::fake_rbd_cli_race_child",
                "--nocapture",
            ])
            .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
            .env("ODOROBO_FAKE_RBD_CHILD", "1")
            .env("ODOROBO_FAKE_RBD_STATE", &state)
            .env("ODOROBO_FAKE_RBD_LOG", &log)
            .output()
            .expect("run fake RBD CLI regression child");
        assert!(
            output.status.success(),
            "fake RBD child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(root).expect("remove fake RBD fixture");
    }

    #[test]
    fn fake_rbd_cli_duplicate_same_image_rows_fail_closed_in_either_order() {
        let root = std::env::temp_dir().join(format!(
            "odorobo-fake-rbd-duplicate-{}",
            ulid::Ulid::generate()
        ));
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create fake RBD directory");
        let cli = bin_dir.join("rbd");
        std::fs::write(
            &cli,
            r#"#!/bin/sh
state="$ODOROBO_FAKE_RBD_STATE"
log="$ODOROBO_FAKE_RBD_LOG"
if [ "$1" = "device" ] && [ "$2" = "list" ]; then
    cat "$state"
    exit 0
fi
if [ "$1" = "device" ] && [ "$2" = "map" ]; then
    if [ "$ODOROBO_FAKE_RBD_ORDER" = "B_FIRST" ]; then
        printf '[{"pool":"pool","namespace":"","name":"image","snap":"-","device":"/dev/rbdB"},{"pool":"pool","namespace":"","name":"image","snap":"-","device":"/dev/rbdC"}]' > "$state"
    else
        printf '[{"pool":"pool","namespace":"","name":"image","snap":"-","device":"/dev/rbdC"},{"pool":"pool","namespace":"","name":"image","snap":"-","device":"/dev/rbdB"}]' > "$state"
    fi
    printf '/dev/rbdB\n'
    exit 0
fi
if [ "$1" = "device" ] && [ "$2" = "unmap" ]; then
    printf 'UNMAP %s\n' "$5" >> "$log"
    echo 'unexpected unmap of ambiguous mapping' >&2
    exit 2
fi
echo 'unexpected rbd command' >&2
exit 2
"#,
        )
        .expect("write fake RBD CLI");
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755))
            .expect("make fake RBD executable");
        let state = root.join("mappings.json");
        let log = root.join("commands.log");
        for order in ["B_FIRST", "C_FIRST"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ch_driver::transform::storage::rbd::tests::fake_rbd_cli_duplicate_lifecycle_child",
                    "--nocapture",
                ])
                .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
                .env("ODOROBO_FAKE_RBD_DUPLICATE_CHILD", "1")
                .env("ODOROBO_FAKE_RBD_ORDER", order)
                .env("ODOROBO_FAKE_RBD_STATE", &state)
                .env("ODOROBO_FAKE_RBD_LOG", &log)
                .output()
                .expect("run fake duplicate-map RBD regression child");
            assert!(
                output.status.success(),
                "fake duplicate-map RBD child ({order}) failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::remove_dir_all(root).expect("remove fake RBD fixture");
    }

    #[tokio::test]
    async fn fake_rbd_cli_duplicate_lifecycle_child() {
        if std::env::var_os("ODOROBO_FAKE_RBD_DUPLICATE_CHILD").is_none() {
            return;
        }
        let state = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_RBD_STATE").expect("fake RBD state path"),
        );
        let log = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_RBD_LOG").expect("fake RBD log path"),
        );
        let order = std::env::var("ODOROBO_FAKE_RBD_ORDER").expect("fake row order");
        let backend = RbdStorage;
        let uri = Url::parse("rbd://pool/image").unwrap();
        std::fs::write(&state, "[]").expect("clear fake mappings");
        std::fs::write(&log, "").expect("clear fake command log");
        let mut persisted = None;
        let mut persist = |device: &str| {
            persisted = Some(device.to_owned());
            Ok(())
        };
        let error = backend
            .resolve_claimed(&uri, true, None, &mut persist)
            .await
            .expect_err("multiple matching image rows make acquisition ownership ambiguous");
        assert!(error.to_string().contains("matching image rows"));
        assert_eq!(persisted, None, "do not pin an ambiguous acquisition");
        let error = backend
            .release_claimed(&uri, None)
            .await
            .expect_err("ambiguous unpinned acquisition must retain its claim");
        assert!(error.to_string().contains("without its pinned device"));
        let mappings = std::fs::read_to_string(&state).expect("read ambiguous mappings");
        assert!(mappings.contains("/dev/rbdB"), "{order}: B survives");
        assert!(mappings.contains("/dev/rbdC"), "{order}: C survives");
        assert!(
            std::fs::read_to_string(&log)
                .expect("read unmap log")
                .is_empty(),
            "{order}: cleanup never unmaps either ambiguous mapping"
        );
    }

    #[test]
    fn successful_release_with_recycled_kernel_path_is_not_retried_after_restart() {
        let root = std::env::temp_dir().join(format!(
            "odorobo-fake-rbd-recycle-{}",
            ulid::Ulid::generate()
        ));
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create fake RBD directory");
        let cli = bin_dir.join("rbd");
        std::fs::write(
            &cli,
            r#"#!/bin/sh
state="$ODOROBO_FAKE_RBD_STATE"
log="$ODOROBO_FAKE_RBD_LOG"
if [ "$1" = "device" ] && [ "$2" = "list" ]; then
    cat "$state"
    exit 0
fi
if [ "$1" = "device" ] && [ "$2" = "map" ]; then
    printf '[{"pool":"pool","namespace":"","name":"image","snap":"-","device":"/dev/rbd0"}]' > "$state"
    printf '/dev/rbd0\n'
    exit 0
fi
if [ "$1" = "device" ] && [ "$2" = "unmap" ]; then
    printf 'UNMAP %s\n' "$5" >> "$log"
    printf '[]' > "$state"
    exit 0
fi
echo 'unexpected rbd command' >&2
exit 2
"#,
        )
        .expect("write fake RBD CLI");
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755))
            .expect("make fake RBD executable");
        let state = root.join("mappings.json");
        let log = root.join("commands.log");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ch_driver::transform::storage::rbd::tests::fake_rbd_recycle_lifecycle_child",
                "--nocapture",
            ])
            .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
            .env("ODOROBO_FAKE_RBD_RECYCLE_CHILD", "1")
            .env("ODOROBO_FAKE_RBD_STATE", &state)
            .env("ODOROBO_FAKE_RBD_LOG", &log)
            .output()
            .expect("run fake RBD recycled-device child");
        assert!(
            output.status.success(),
            "fake RBD recycled-device child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(root).expect("remove fake RBD fixture");
    }

    async fn assert_recovered_mapping_is_not_unmapped(
        backend: &RbdStorage,
        uri: &Url,
        log: &std::path::Path,
    ) {
        let state = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_RBD_STATE").expect("fake RBD state path"),
        );
        std::fs::write(
            &state,
            r#"[{"pool":"pool","namespace":"","name":"image","snap":"-","device":"/dev/rbd0"}]"#,
        )
        .expect("simulate recovered mapping with a recycled kernel path");
        std::fs::write(log, "").expect("initialize fake RBD log");
        let error = backend
            .release_claimed_with_intent(
                uri,
                Some("/dev/rbd0"),
                StorageReleaseState {
                    recovered: true,
                    ..Default::default()
                },
            )
            .await
            .expect_err("a recovered tuple cannot prove mapping incarnation");
        assert!(error.to_string().contains("release is uncertain"));
        assert!(std::fs::read_to_string(log).unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fake_rbd_recycle_lifecycle_child() {
        if std::env::var_os("ODOROBO_FAKE_RBD_RECYCLE_CHILD").is_none() {
            return;
        }
        let state = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_RBD_STATE").expect("fake RBD state path"),
        );
        let log = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_RBD_LOG").expect("fake RBD log path"),
        );
        let uri = Url::parse("rbd://pool/image").unwrap();
        let backend = RbdStorage;
        assert_recovered_mapping_is_not_unmapped(&backend, &uri, &log).await;

        std::fs::write(&state, "[]").expect("initialize fresh acquisition state");
        let mut persisted = None;
        let mut persist = |device: &str| {
            persisted = Some(device.to_owned());
            Ok(())
        };
        let resolved = backend
            .resolve_claimed(&uri, true, None, &mut persist)
            .await
            .expect("acquire and persist exact kernel device");
        assert_eq!(resolved, PathBuf::from("/dev/rbd0"));
        let device = persisted.expect("persisted acquisition identity");

        // Simulate the durable release-intent record used across a restart.
        // The first backend release succeeds, but forgetting the cleanup journal
        // fails, leaving the intent record available for restart recovery.
        let intent_file = state.with_extension("release-intent");
        let mut attachment = |_: &Url, _: &str| Ok(Some(device.clone()));
        let mut begin_release = |_: &Url, _: &str| {
            if intent_file.exists() {
                Ok(StorageReleaseState {
                    already_started: true,
                    recovered: false,
                    claim_exists: true,
                })
            } else {
                std::fs::write(&intent_file, "started")
                    .expect("persist release intent before the side effect");
                Ok(StorageReleaseState::default())
            }
        };
        let mut forget = |_: &Url, _: &str| Err(eyre!("simulated interrupted journal persistence"));
        let mut cleanup = StorageCleanupContext {
            attachment: &mut attachment,
            begin_release: &mut begin_release,
            forget: &mut forget,
        };
        let transformer = StorageDriverTransformer::new().with_backend(RbdStorage);
        let mut config = VmConfig::default();
        let error = transformer
            .teardown_with_storage_ownership(
                "recycle-test",
                &mut config,
                std::slice::from_ref(&uri),
                &mut cleanup,
            )
            .expect_err("release succeeds but interrupted journal forget is reported");
        assert!(
            error
                .to_string()
                .contains("interrupted journal persistence")
        );
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "[]");
        assert!(
            intent_file.exists(),
            "release intent survives the interruption"
        );

        // An external actor remaps the same image and the kernel reuses the
        // identical /dev/rbd0 name. Restart cleanup must retain uncertainty
        // instead of unmapping the new occupant of the recycled pathname.
        std::fs::write(
            &state,
            r#"[{"pool":"pool","namespace":"","name":"image","snap":"-","device":"/dev/rbd0"}]"#,
        )
        .expect("simulate external same-image recycled device");
        let error = transformer
            .teardown_with_storage_ownership(
                "recycle-test",
                &mut config,
                std::slice::from_ref(&uri),
                &mut cleanup,
            )
            .expect_err("recycled identity must remain uncertain");
        assert!(error.to_string().contains("release is uncertain"));
        assert!(
            std::fs::read_to_string(&state)
                .unwrap()
                .contains("/dev/rbd0")
        );
        assert_eq!(
            std::fs::read_to_string(&log).expect("read unmap log"),
            "UNMAP /dev/rbd0\n"
        );
    }

    #[tokio::test]
    async fn fake_rbd_cli_race_child() {
        if std::env::var_os("ODOROBO_FAKE_RBD_CHILD").is_none() {
            return;
        }
        let state = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_RBD_STATE").expect("fake RBD state path"),
        );
        let log = std::path::PathBuf::from(
            std::env::var_os("ODOROBO_FAKE_RBD_LOG").expect("fake RBD log path"),
        );
        std::fs::write(&state, "[]").expect("initialize fake mappings");
        std::fs::write(&log, "").expect("initialize fake RBD log");
        let uri = Url::parse("rbd://pool/image").unwrap();
        let backend = RbdStorage;

        // A's exclusive check observes no mapping. A competing actor then maps
        // the image before A's acquisition command, matching the old check,
        // journal, then second-check race.
        backend
            .ensure_exclusive(&uri)
            .await
            .expect("A precheck sees unmapped");
        let output = tokio::process::Command::new("rbd")
            .args(["device", "map", "pool/image"])
            .output()
            .await
            .expect("fake competing map command");
        assert!(output.status.success());

        let mut persist = |_identity: &str| Ok(());
        let error = backend
            .resolve_claimed(&uri, true, None, &mut persist)
            .await
            .expect_err("A must not adopt the competing mapping");
        assert!(error.to_string().contains("rbd map failed"));
        let error = backend
            .release_claimed(&uri, None)
            .await
            .expect_err("an unpinned attempt must not unmap another actor's mapping");
        assert!(error.to_string().contains("without its pinned device"));
        let mappings = std::fs::read_to_string(&state).expect("read competing map state");
        assert!(mappings.contains("/dev/rbdB"));
        assert!(
            !std::fs::read_to_string(&log)
                .expect("read fake RBD commands")
                .contains("UNMAP")
        );
    }

    #[test]
    fn test_rbd_image_from_uri() {
        let uri = Url::parse("rbd://my-pool/my-image").unwrap();
        let image = RbdImage::try_from(&uri).unwrap();
        assert_eq!(image.pool, "my-pool");
        assert_eq!(image.image, "my-image");
        assert_eq!(image.rbd_path(), "my-pool/my-image");
    }

    #[test]
    fn rbd_json_mapping_parser_preserves_namespace_and_snapshot_identity() {
        let input = r#"[
            {"id":0,"pool":"pool","namespace":"","name":"disk","snap":"-","device":"/dev/rbd0"},
            {"id":1,"pool":"pool","namespace":"ns","name":"disk","snap":"-","device":"/dev/rbd1"},
            {"id":2,"pool":"pool","namespace":"","name":"disk","snap":"snap1","device":"/dev/rbd2"}
        ]"#;
        let mappings = rbd_lines_list(input).unwrap();
        assert_eq!(
            mappings,
            [
                ("pool/disk".to_owned(), "/dev/rbd0".to_owned()),
                ("pool/ns/disk".to_owned(), "/dev/rbd1".to_owned()),
                ("pool/disk@snap1".to_owned(), "/dev/rbd2".to_owned()),
            ]
        );
    }

    #[test]
    fn rbd_mapping_identity_prevents_skipping_requested_snapshot_unmap() {
        let mappings = rbd_lines_list(
            r#"[
                {"id":0,"pool":"pool","namespace":"","name":"disk","snap":"-","device":"/dev/rbd0"},
                {"id":1,"pool":"pool","namespace":"","name":"disk","snap":"snap1","device":"/dev/rbd1"}
            ]"#,
        )
        .unwrap();

        assert!(rbd_mapping_exists(&mappings, "pool/disk@snap1"));
        assert!(rbd_mapping_exists(&mappings, "pool/disk"));
        assert!(!rbd_mapping_exists(&mappings, "pool/disk@snap2"));
    }

    #[test]
    fn malformed_rbd_json_is_an_error_not_an_empty_mapping_list() {
        let _error = rbd_lines_list("not json").unwrap_err();
        assert!(
            rbd_lines_list(r#"[{"pool":"pool","name":"disk","device":"/dev/rbd0"}]"#).is_err(),
            "missing namespace or snapshot fields make mapping state uncertain"
        );
    }
}
