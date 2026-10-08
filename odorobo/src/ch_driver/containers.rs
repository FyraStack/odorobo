//! OCI container microVM roots served over virtiofs. All OCI, composefs,
//! OverlayFS, fs-verity, and persistence preparation happens on the host; the
//! guest sees an ordinary virtiofs root and does not need composefs support.
//!
//! Skopeo layouts keep per-reference metadata while verified compressed blobs
//! are hard-linked into a node-local digest CAS. Each unique OCI layer is
//! extracted temporarily, translated to OverlayFS whiteout semantics, and
//! built as an immutable fs-verity composefs image with a per-layer object
//! store. Temporary extraction is deleted. Per-VM composefs mounts are stacked
//! in OCI order with OverlayFS before actor-supervised virtiofsd exports them.
//! Security notes: layers are unpacked by this process as root; tar paths
//! are validated (`..` rejected) and every unpack/whiteout operation refuses
//! to traverse symlinks planted by earlier layers, so a hostile image cannot
//! write through or remove host paths. The store must live on an
//! fs-verity-capable filesystem (ext4, btrfs) for digest-pinned mounts.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use sha2::Digest;
use stable_eyre::{
    Result,
    eyre::{Context, bail, eyre},
};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

use crate::manifest::{Rootfs, RootfsMode};

/// virtiofs mount tag the guest root is served under.
pub const ROOTFS_TAG: &str = "rootfs";
/// The tiny custom microvm kernel built by
/// `scripts/build-microvm-kernel.sh` (issue #112 buildconfig).
pub const MICROVM_KERNEL_PATH: &str = "/var/lib/odorobo/microvm/vmlinux";
/// Direct-kernel-boot cmdline for a composefs/virtiofs root. The guest init
/// must live at /sbin/init, /etc/init, /bin/init or /bin/sh (kernel default).
/// console=ttyS0 (the 16550 UART) because the driver's ConsoleTransform
/// sockets the UART; the virtio-console (hvc0) is off.
pub const DEFAULT_ROOTFS_CMDLINE: &str = "console=ttyS0 root=rootfs rootfstype=virtiofs rw";

const OCI_CACHE_ROOT: &str = "/var/lib/odorobo/oci-cache";
const OCI_BLOBS_ROOT: &str = "/var/lib/odorobo/oci-blobs/sha256";
const PERSISTENT_ROOTFS_ROOT: &str = "/var/lib/odorobo/persistent-rootfs";
const SKOPEO_TIMEOUT: Duration = Duration::from_secs(600);

/// Remove temporary external-tool artifacts even if their async operation is
/// cancelled. Large directory cleanup is moved to Tokio's blocking pool.
struct RemovePathOnDrop(Option<PathBuf>);

impl RemovePathOnDrop {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for RemovePathOnDrop {
    fn drop(&mut self) {
        let Some(path) = self.0.take() else {
            return;
        };
        let cleanup = move || match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            _ = handle.spawn_blocking(cleanup);
        } else {
            _ = cleanup();
        }
    }
}

/// A rootfs made ready for a specific VM.
#[derive(Debug)]
pub struct PreparedRootfs {
    /// OCI manifest digest identifying the immutable base.
    pub digest: String,
    /// This VM's mount point of the rootfs (virtiofsd shared-dir).
    pub mount: PathBuf,
    /// Per-VM overlayfs upper directory when the root is writable.
    pub upper: Option<PathBuf>,
    /// Per-VM overlayfs work directory when the root is writable.
    pub work: Option<PathBuf>,
    /// Per-layer composefs mounts backing the merged OverlayFS lower.
    layer_mounts: Vec<PathBuf>,
    /// Persistent backend mount directory; populated only for RBD state.
    persistent_state_mount: Option<PathBuf>,
    /// RBD image identity for explicit deletion.
    rbd_image: Option<String>,
    /// Node-local flock prevents duplicate actor instances for this VM id.
    _state_lock: Arc<std::fs::File>,
    /// Persistence policy controls cleanup and writable scratch behavior.
    pub mode: RootfsMode,
    /// Host tmpfs mounts used for the bounded read-only-mode scratch paths.
    scratch_mounts: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// OCI pull + unpack
// ---------------------------------------------------------------------------

/// Filesystem-safe slug for an image reference (cache dir / cache key).
/// Always a single, non-traversing path component.
fn sanitize_image_ref(image_ref: &str) -> String {
    let slug: String = image_ref
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    match slug.as_str() {
        "" | "." | ".." => "_".to_owned(),
        _ => slug.chars().take(96).collect(),
    }
}

/// Accepts bare docker refs and explicit transports alike.
fn normalize_image_ref(image_ref: &str) -> String {
    const TRANSPORTS: [&str; 5] = [
        "docker://",
        "oci:",
        "dir:",
        "containers-storage:",
        "docker-archive:",
    ];
    if TRANSPORTS.iter().any(|t| image_ref.starts_with(t)) {
        image_ref.to_owned()
    } else {
        format!("docker://{image_ref}")
    }
}

/// Media type of an OCI layer blob -> how it is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    Gzip,
    Zstd,
    None,
}

#[derive(Clone, Copy, Debug)]
struct OciUnpackLimits {
    max_compressed_bytes: u64,
    max_layer_bytes: u64,
    max_image_bytes: u64,
    max_image_entries: u64,
}

impl OciUnpackLimits {
    fn from_env() -> Result<Self> {
        Ok(Self {
            max_compressed_bytes: parse_size_limit(
                "ODOROBO_ROOTFS_MAX_COMPRESSED_BYTES",
                "16G",
                1024_u64.pow(4),
            )?,
            max_layer_bytes: parse_size_limit(
                "ODOROBO_ROOTFS_MAX_LAYER_BYTES",
                "8G",
                1024_u64.pow(4),
            )?,
            max_image_bytes: parse_size_limit(
                "ODOROBO_ROOTFS_MAX_IMAGE_BYTES",
                "32G",
                1024_u64.pow(4),
            )?,
            max_image_entries: parse_entry_limit("ODOROBO_ROOTFS_MAX_ENTRIES", 1_000_000)?,
        })
    }
}

fn parse_size_limit(env: &str, default: &str, maximum: u64) -> Result<u64> {
    let value = std::env::var(env).unwrap_or_else(|_| default.to_owned());
    parse_size_bytes(env, &value, maximum, true)
}

fn parse_size_bytes(env: &str, value: &str, maximum: u64, allow_terabytes: bool) -> Result<u64> {
    let value = value.trim();
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (digits, unit) = value.split_at(split);
    let count = digits
        .parse::<u64>()
        .map_err(|_| eyre!("invalid size limit {env}={value:?}"))?;
    let multiplier = match unit.to_ascii_uppercase().as_str() {
        "M" | "MB" => 1024_u64.pow(2),
        "G" | "GB" => 1024_u64.pow(3),
        "T" | "TB" if allow_terabytes => 1024_u64.pow(4),
        _ => bail!("unsupported size unit for {env}: {unit:?}"),
    };
    let bytes = count
        .checked_mul(multiplier)
        .ok_or_else(|| eyre!("size limit {env} overflows"))?;
    if bytes < 1024 * 1024 || bytes > maximum {
        bail!("{env} must be between 1M and {} bytes", maximum);
    }
    Ok(bytes)
}

fn parse_entry_limit(env: &str, default: u64) -> Result<u64> {
    let value = std::env::var(env).unwrap_or_else(|_| default.to_string());
    let count = value
        .parse::<u64>()
        .map_err(|_| eyre!("invalid entry limit {env}={value:?}"))?;
    if !(1..=10_000_000).contains(&count) {
        bail!("{env} must be between 1 and 10,000,000");
    }
    Ok(count)
}

fn compression_of_media_type(media_type: &str) -> Option<Compression> {
    // OCI and Docker layer media types; tar is the uncompressed variant.
    const TAR: [&str; 2] = [
        "application/vnd.oci.image.layer.v1.tar",
        "application/vnd.docker.image.rootfs.diff.tar",
    ];
    const GZIP: [&str; 2] = [
        "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.docker.image.rootfs.diff.tar.gzip",
    ];
    const ZSTD: [&str; 2] = [
        "application/vnd.oci.image.layer.v1.tar+zstd",
        "application/vnd.docker.image.rootfs.diff.tar.zstd",
    ];
    if TAR.contains(&media_type) {
        Some(Compression::None)
    } else if GZIP.contains(&media_type) {
        Some(Compression::Gzip)
    } else if ZSTD.contains(&media_type) {
        Some(Compression::Zstd)
    } else {
        None
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest_blob_path(layout: &Path, digest: &str) -> Result<PathBuf> {
    let (algorithm, hex) = digest
        .split_once(':')
        .ok_or_else(|| eyre!("malformed digest {digest:?}"))?;
    if algorithm != "sha256" {
        bail!("unsupported digest algorithm {algorithm:?}");
    }
    // The hex comes from manifests (possibly registry-controlled); never let
    // it traverse out of the blobs dir.
    if hex.len() != 64
        || !hex
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        bail!("malformed sha256 digest hex in {digest:?}");
    }
    Ok(layout.join("blobs").join(algorithm).join(hex))
}

/// Publish each layout blob into a digest-addressed node-local hard-link CAS.
/// Reference layouts retain their metadata, but shared blob inodes prevent
/// identical compressed layers from consuming disk repeatedly.
fn globalize_layout_blobs(layout: &Path) -> Result<()> {
    globalize_layout_blobs_at(layout, Path::new(OCI_BLOBS_ROOT))
}

fn globalize_layout_blobs_at(layout: &Path, cas_root: &Path) -> Result<()> {
    let blobs = layout.join("blobs/sha256");
    if !blobs.is_dir() {
        bail!(
            "OCI layout {} has no sha256 blob directory",
            layout.display()
        );
    }
    std::fs::create_dir_all(cas_root)?;
    for entry in std::fs::read_dir(&blobs)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_file() {
            bail!(
                "OCI blob entry {} is not a regular file",
                entry.path().display()
            );
        }
        let filename = entry.file_name().to_string_lossy().into_owned();
        if filename.len() != 64
            || !filename
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            bail!("malformed OCI blob filename {filename:?}");
        }
        let digest = format!("sha256:{filename}");
        let blob = entry.path();
        if hash_file(&blob)? != digest {
            bail!("OCI blob {filename} does not match its digest");
        }
        let cas = cas_root.join(&filename);
        if cas.exists() {
            if !std::fs::symlink_metadata(&cas)?.file_type().is_file() {
                bail!(
                    "node-local OCI CAS entry {} is not a regular file",
                    cas.display()
                );
            }
            if hash_file(&cas)? != digest {
                bail!(
                    "node-local OCI CAS entry {} failed digest verification",
                    cas.display()
                );
            }
        } else {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&blob, permissions)?;
            match std::fs::hard_link(&blob, &cas) {
                Ok(()) => {}
                Err(err) if cas.exists() && hash_file(&cas)? == digest => {
                    _ = err;
                }
                Err(err) => {
                    return Err(eyre!(
                        "publish OCI blob to node CAS {}: {err}",
                        cas.display()
                    ));
                }
            }
        }
        if !std::fs::metadata(&blob)?.permissions().readonly() {
            let mut permissions = std::fs::metadata(&blob)?.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&blob, permissions)?;
        }
        if std::fs::metadata(&blob)?.ino() != std::fs::metadata(&cas)?.ino() {
            std::fs::remove_file(&blob)?;
            std::fs::hard_link(&cas, &blob)?;
        }
    }
    Ok(())
}

fn oci_layout_blob_bytes(layout: &Path) -> Result<u64> {
    let blobs = layout.join("blobs/sha256");
    let entries = match std::fs::read_dir(&blobs) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    entries.into_iter().try_fold(0_u64, |sum, entry| {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if !meta.file_type().is_file() {
            bail!("OCI blob {} is not a regular file", path.display());
        }
        sum.checked_add(meta.len())
            .ok_or_else(|| eyre!("OCI blob bytes overflow"))
    })
}

fn set_child_file_size_limit(command: &mut Command, limit_bytes: u64) -> Result<()> {
    let limit: libc::rlim_t = limit_bytes
        .try_into()
        .map_err(|_| eyre!("file size limit exceeds RLIMIT_FSIZE"))?;
    // SAFETY: the child hook calls only setrlimit before exec and captures a
    // plain scalar; it does not access Tokio state or allocate in the child.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            let limits = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limits) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

async fn inspect_raw_manifest(image_ref: &str) -> Result<Vec<u8>> {
    const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
    let path =
        std::env::temp_dir().join(format!(".odorobo-manifest-{}.json", ulid::Ulid::generate()));
    let _cleanup = RemovePathOnDrop::new(path.clone());
    let file = std::fs::File::create(&path)?;
    let mut command = Command::new("skopeo");
    command
        .args(["inspect", "--raw", image_ref])
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    set_child_file_size_limit(&mut command, MAX_MANIFEST_BYTES)?;
    let mut child = command
        .spawn()
        .wrap_err("failed to start skopeo manifest preflight")?;
    let status = match tokio::time::timeout(Duration::from_secs(60), child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            _ = child.start_kill();
            _ = child.wait().await;
            bail!("skopeo manifest preflight timed out");
        }
    };
    if !status.success() {
        bail!("skopeo inspect --raw failed for {image_ref} ({status})");
    }
    tokio::fs::read(&path).await.map_err(Into::into)
}

fn docker_digest_ref(image_ref: &str, digest: &str) -> Result<String> {
    let name = image_ref
        .strip_prefix("docker://")
        .ok_or_else(|| eyre!("cannot resolve Docker digest reference {image_ref:?}"))?;
    let repository = if let Some((repository, _tag_or_digest)) = name.split_once('@') {
        repository
    } else {
        let slash = name.rfind('/').map_or(0, |index| index + 1);
        match name[slash..].rfind(':') {
            Some(index) => &name[..slash + index],
            None => name,
        }
    };
    if repository.is_empty() {
        bail!("empty Docker repository in {image_ref:?}");
    }
    Ok(format!("docker://{repository}@{digest}"))
}

fn preflight_local_oci_layout(normalized: &str) -> Result<(String, u64)> {
    let source = normalized
        .strip_prefix("oci:")
        .ok_or_else(|| eyre!("not an OCI layout transport"))?;
    let (layout, _tag) = source
        .rsplit_once(':')
        .ok_or_else(|| eyre!("OCI layout reference has no tag separator"))?;
    let layout = Path::new(layout);
    let (manifest_digest, layers) = read_oci_manifest(layout)?;
    let manifest_path = digest_blob_path(layout, &manifest_digest)?;
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or_else(|| eyre!("OCI manifest has no config digest"))?;
    let config_path = digest_blob_path(layout, config_digest)?;
    let mut total = std::fs::metadata(&manifest_path)?
        .len()
        .checked_add(std::fs::metadata(&config_path)?.len())
        .ok_or_else(|| eyre!("compressed OCI metadata size overflows"))?;
    let mut seen = std::collections::HashSet::new();
    for layer in layers {
        if seen.insert(layer.digest) {
            total = total
                .checked_add(layer.size.ok_or_else(|| eyre!("OCI layer has no size"))?)
                .ok_or_else(|| eyre!("compressed OCI layer size overflows"))?;
        }
    }
    Ok((manifest_digest, total))
}

struct PullPreflight {
    copy_ref: String,
    manifest_digest: String,
    compressed_bytes: u64,
}

fn compressed_manifest_size(manifest: &serde_json::Value, manifest_bytes: u64) -> Result<u64> {
    let layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| eyre!("selected OCI image manifest has no layers"))?;
    let config_bytes = manifest["config"]["size"]
        .as_u64()
        .ok_or_else(|| eyre!("OCI config descriptor has no valid size"))?;
    let mut total = manifest_bytes
        .checked_add(config_bytes)
        .ok_or_else(|| eyre!("compressed OCI size overflows"))?;
    let mut seen = std::collections::HashSet::new();
    for layer in layers {
        let digest = layer["digest"]
            .as_str()
            .ok_or_else(|| eyre!("OCI layer descriptor has no digest"))?;
        let size = layer["size"]
            .as_u64()
            .ok_or_else(|| eyre!("OCI layer descriptor has no size"))?;
        if seen.insert(digest) {
            total = total
                .checked_add(size)
                .ok_or_else(|| eyre!("compressed OCI size overflows"))?;
        }
    }
    Ok(total)
}

async fn preflight_compressed_size(
    image_ref: &str,
    normalized: &str,
    max_bytes: u64,
) -> Result<PullPreflight> {
    let raw = inspect_raw_manifest(normalized).await?;
    let raw_len = raw.len() as u64;
    let value: serde_json::Value =
        serde_json::from_slice(&raw).wrap_err("parse raw OCI manifest during size preflight")?;
    if value["layers"].as_array().is_none()
        && value["manifests"].as_array().is_some()
        && normalized.starts_with("oci:")
    {
        let local_ref = normalized.to_owned();
        let (manifest_digest, total) =
            tokio::task::spawn_blocking(move || preflight_local_oci_layout(&local_ref))
                .await
                .map_err(|e| eyre!("local OCI size preflight task panicked: {e}"))??;
        if total > max_bytes {
            bail!(
                "OCI source {image_ref} advertises {total} compressed bytes, exceeding configured limit {max_bytes}"
            );
        }
        return Ok(PullPreflight {
            copy_ref: normalized.to_owned(),
            manifest_digest,
            compressed_bytes: total,
        });
    }
    let (manifest, manifest_bytes, copy_ref, manifest_digest) = if value["layers"]
        .as_array()
        .is_some()
    {
        let digest = format!("sha256:{}", to_hex(&sha2::Sha256::digest(&raw)));
        let copy_ref = if normalized.starts_with("docker://") {
            docker_digest_ref(normalized, &digest)?
        } else {
            normalized.to_owned()
        };
        (value, raw_len, copy_ref, digest)
    } else if let Some(manifests) = value["manifests"].as_array() {
        let host_arch = normalized_arch(std::env::consts::ARCH);
        let compatible = manifests
            .iter()
            .filter(|entry| {
                entry["platform"]["os"].as_str() == Some("linux")
                    && entry["platform"]["architecture"]
                        .as_str()
                        .is_some_and(|arch| normalized_arch(arch) == host_arch)
            })
            .collect::<Vec<_>>();
        if compatible.len() != 1 {
            bail!(
                "cannot preflight OCI index {image_ref}: found {} manifests for linux/{host_arch}",
                compatible.len()
            );
        }
        let digest = compatible[0]["digest"]
            .as_str()
            .ok_or_else(|| eyre!("OCI index descriptor has no digest"))?;
        let digest_ref = docker_digest_ref(normalized, digest)?;
        let manifest_raw = inspect_raw_manifest(&digest_ref).await?;
        let actual_digest = format!("sha256:{}", to_hex(&sha2::Sha256::digest(&manifest_raw)));
        if actual_digest != digest {
            bail!(
                "selected OCI manifest digest mismatch during preflight: expected {digest}, got {actual_digest}"
            );
        }
        let bytes = manifest_raw.len() as u64;
        (
            serde_json::from_slice(&manifest_raw).wrap_err("parse selected raw OCI manifest")?,
            bytes,
            digest_ref,
            digest.to_owned(),
        )
    } else {
        bail!("raw OCI response for {image_ref} is neither an image manifest nor an index");
    };
    let compressed_bytes = compressed_manifest_size(&manifest, manifest_bytes)?;
    if compressed_bytes > max_bytes {
        bail!(
            "OCI source {image_ref} advertises {compressed_bytes} compressed bytes, exceeding configured limit {max_bytes}"
        );
    }
    Ok(PullPreflight {
        copy_ref,
        manifest_digest,
        compressed_bytes,
    })
}

/// Pull the image reference into the per-ref OCI layout cache; returns the
/// layout dir. A metadata preflight, serial layer copy and writer file-size
/// limit constrain the temporary pull before it is accepted; tags remain cached until cleared.
async fn pull_oci(image_ref: &str, max_compressed_bytes: u64) -> Result<PathBuf> {
    let image_ref = image_ref.to_owned();
    // Cancellation of an actor must not concurrently remove skopeo's staging
    // tree. The pull owns its child and cleanup until termination is reaped.
    tokio::spawn(async move { pull_oci_inner(&image_ref, max_compressed_bytes).await })
        .await
        .map_err(|err| eyre!("OCI pull task failed: {err}"))?
}

async fn pull_oci_inner(image_ref: &str, max_compressed_bytes: u64) -> Result<PathBuf> {
    let normalized = normalize_image_ref(image_ref);
    // Keep human-readable names but include the full ref hash so sanitization
    // cannot alias distinct registry names into the same cached OCI layout.
    let ref_hash = to_hex(&sha2::Sha256::digest(image_ref.as_bytes()));
    let dir =
        PathBuf::from(OCI_CACHE_ROOT).join(format!("{}-{ref_hash}", sanitize_image_ref(image_ref)));
    if dir.join("index.json").exists() {
        let layout = dir.clone();
        let bytes = tokio::task::spawn_blocking(move || -> Result<_> {
            globalize_layout_blobs(&layout)?;
            oci_layout_blob_bytes(&layout)
        })
        .await
        .map_err(|e| eyre!("OCI layout cache task panicked: {e}"))??;
        if bytes > max_compressed_bytes {
            bail!(
                "cached OCI blobs for {normalized} use {bytes} bytes, exceeding configured limit {max_compressed_bytes}"
            );
        }
        debug!(image_ref, dir = %dir.display(), compressed_blob_bytes=bytes, "OCI layout cache hit");
        return Ok(dir);
    }
    let preflight = preflight_compressed_size(image_ref, &normalized, max_compressed_bytes).await?;
    tokio::fs::create_dir_all(&dir)
        .await
        .wrap_err_with(|| format!("create OCI cache dir {}", dir.display()))?;
    let pull_id = ulid::Ulid::generate();
    let tmp = dir.with_file_name(format!(".pull-{pull_id}"));
    let stderr_path = dir.with_file_name(format!(".pull-{pull_id}.stderr"));
    let mut tmp_cleanup = RemovePathOnDrop::new(tmp.clone());
    let mut stderr_cleanup = RemovePathOnDrop::new(stderr_path.clone());
    let tag_ref = format!("oci:{}:rootfs", tmp.display());

    info!(image_ref, dir = %dir.display(), preflight_compressed_bytes=preflight.compressed_bytes, copy_ref=%preflight.copy_ref, "pulling digest-preflighted OCI image with skopeo");
    let stderr_file = std::fs::File::create(&stderr_path)?;
    let mut command = Command::new("skopeo");
    command
        .args([
            "copy",
            "--remove-signatures",
            "--preserve-digests",
            "--image-parallel-copies=1",
            &preflight.copy_ref,
            &tag_ref,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(stderr_file)
        .kill_on_drop(true);
    set_child_file_size_limit(&mut command, max_compressed_bytes)?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            _ = tokio::fs::remove_file(&stderr_path).await;
            return Err(err)
                .wrap_err("failed to run skopeo (is it installed? `dnf install skopeo`)");
        }
    };
    let pull = async {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(status) = child.try_wait()? {
                let layout = tmp.clone();
                let bytes = tokio::task::spawn_blocking(move || oci_layout_blob_bytes(&layout))
                    .await
                    .map_err(|e| eyre!("OCI pull size check task panicked: {e}"))??;
                if bytes > max_compressed_bytes {
                    bail!(
                        "OCI pull of {normalized} reached {bytes} compressed bytes, exceeding limit {max_compressed_bytes}"
                    );
                }
                return Ok(status);
            }
            let layout = tmp.clone();
            let bytes = tokio::task::spawn_blocking(move || oci_layout_blob_bytes(&layout))
                .await
                .map_err(|e| eyre!("OCI pull size check task panicked: {e}"))??;
            if bytes > max_compressed_bytes {
                _ = child.start_kill();
                _ = child.wait().await;
                bail!(
                    "OCI pull of {normalized} exceeded compressed-byte limit {max_compressed_bytes}"
                );
            }
        }
    };
    let status = match tokio::time::timeout(SKOPEO_TIMEOUT, pull).await {
        Ok(Ok(status)) => status,
        Ok(Err(err)) => {
            _ = child.start_kill();
            _ = child.wait().await;
            _ = tokio::fs::remove_dir_all(&tmp).await;
            _ = tokio::fs::remove_file(&stderr_path).await;
            return Err(err);
        }
        Err(_elapsed) => {
            _ = child.start_kill();
            _ = child.wait().await;
            _ = tokio::fs::remove_dir_all(&tmp).await;
            _ = tokio::fs::remove_file(&stderr_path).await;
            bail!("skopeo pull of {normalized} timed out after 600s");
        }
    };
    let stderr = tokio::fs::read_to_string(&stderr_path)
        .await
        .unwrap_or_default();
    _ = tokio::fs::remove_file(&stderr_path).await;
    stderr_cleanup.disarm();
    if !status.success() {
        _ = tokio::fs::remove_dir_all(&tmp).await;
        bail!(
            "skopeo pull of {normalized} failed ({status}): {}",
            stderr.trim()
        );
    }
    let layout_for_check = tmp.clone();
    let expected_manifest = preflight.manifest_digest.clone();
    let selected_manifest = tokio::task::spawn_blocking(move || {
        read_oci_manifest(&layout_for_check).map(|(digest, _)| digest)
    })
    .await
    .map_err(|e| eyre!("post-pull OCI validation task panicked: {e}"))?;
    let selected_manifest = match selected_manifest {
        Ok(digest) => digest,
        Err(err) => {
            _ = tokio::fs::remove_dir_all(&tmp).await;
            return Err(err.wrap_err("validate copied OCI image"));
        }
    };
    if selected_manifest != expected_manifest {
        _ = tokio::fs::remove_dir_all(&tmp).await;
        bail!(
            "copied OCI manifest {selected_manifest} differs from digest-pinned preflight {expected_manifest}"
        );
    }
    let layout_tmp = tmp.clone();
    match tokio::task::spawn_blocking(move || globalize_layout_blobs(&layout_tmp)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            _ = tokio::fs::remove_dir_all(&tmp).await;
            return Err(err.wrap_err("publish verified OCI blobs to node-local CAS"));
        }
        Err(err) => {
            _ = tokio::fs::remove_dir_all(&tmp).await;
            return Err(eyre!("OCI blob CAS task panicked: {err}"));
        }
    }
    match tokio::fs::rename(&tmp, &dir).await {
        Ok(()) => {
            tmp_cleanup.disarm();
            Ok(dir)
        }
        Err(err) if dir.join("index.json").exists() => {
            // Lost a race with another VM pulling the same ref; reuse theirs.
            debug!(?err, "losing pull race, reusing existing layout");
            _ = tokio::fs::remove_dir_all(&tmp).await;
            Ok(dir)
        }
        Err(err) => {
            _ = tokio::fs::remove_dir_all(&tmp).await;
            Err(eyre!("publish pulled layout to {}: {err}", dir.display()))
        }
    }
}

/// Reject tar paths that could escape the scratch tree. `.` components are
/// harmless (GNU tar layers often start with `./`) and are skipped; only
/// `..` (and prefixes) are rejected.
fn safe_rel_path(raw: &Path) -> Option<PathBuf> {
    let stripped = raw.strip_prefix("/").unwrap_or(raw);
    let mut clean = PathBuf::new();
    for component in stripped.components() {
        match component {
            std::path::Component::Normal(part) => clean.push(part),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(clean)
}

fn remove_path(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// Every prefix of `rel` under `tree` must exist and not be a symlink. Tar
/// layers can plant symlinks (`etc -> /etc`); without this guard, directory
/// creation, opaque-whiteout scans and unpacking would resolve through them
/// on the HOST and a hostile image could touch host paths.
fn ensure_no_symlink_ancestors(tree: &Path, rel: &Path) -> Result<()> {
    let mut cur = tree.to_path_buf();
    for component in rel.components() {
        cur.push(component);
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) if meta.is_symlink() => {
                bail!(
                    "tar path {rel:?} traverses symlink {} — refusing (hostile layer?)",
                    cur.display()
                );
            }
            Ok(_) => {}
            // Missing ancestor: nothing below it can be a symlink.
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

/// One OCI whiteout marker deferred until all normal entries in the same layer
/// have been extracted. This makes whiteout behavior independent of tar order.
struct Whiteout {
    parent: PathBuf,
    file_name: String,
}

/// Preserve whiteouts in layer form. OverlayFS supports the trusted.overlay
/// xattr encoding as well as char-device whiteouts; the xattr form works on
/// filesystems and container policies where creating device nodes is forbidden.
fn apply_whiteout(tree: &Path, parent_rel: &Path, file_name: &str) -> Result<()> {
    ensure_no_symlink_ancestors(tree, parent_rel)?;
    let parent = tree.join(parent_rel);
    std::fs::create_dir_all(&parent)
        .wrap_err_with(|| format!("create whiteout parent {}", parent.display()))?;
    if file_name == ".wh..wh..opq" {
        set_overlay_xattr(&parent, "trusted.overlay.opaque", b"y")?;
        debug!(dir = %parent.display(), "preserved opaque whiteout");
    } else {
        let target_name = file_name.strip_prefix(".wh.").unwrap_or(file_name);
        if target_name.is_empty() || target_name == "." || target_name == ".." {
            bail!("invalid OCI whiteout name {file_name:?}");
        }
        let target = parent.join(target_name);
        if std::fs::symlink_metadata(&target).is_ok() {
            bail!(
                "refusing to replace same-layer path with whiteout {}",
                target.display()
            );
        }
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o000)
            .open(&target)
            .wrap_err_with(|| {
                format!("create regular-file overlay whiteout {}", target.display())
            })?;
        set_overlay_xattr(&target, "trusted.overlay.whiteout", b"")?;
        // The opaque=x marker means "this merge dir contains xattr whiteouts"
        // (not opaque=y); OverlayFS uses it to detect these regular files.
        set_overlay_xattr(&parent, "trusted.overlay.opaque", b"x")?;
        debug!(target = %target.display(), "preserved xattr overlay whiteout");
    }
    Ok(())
}

fn set_overlay_xattr(path: &Path, attribute: &str, value: &[u8]) -> Result<()> {
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let name = std::ffi::CString::new(attribute)?;
    // SAFETY: checked NUL-terminated path/name and a valid value buffer.
    let rc = unsafe {
        libc::setxattr(
            cpath.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .wrap_err_with(|| format!("set {attribute} on {}", path.display()));
    }
    Ok(())
}

/// Unpack one non-whiteout tar entry. Whiteouts are returned for deferred
/// application after all files from this same OCI layer have been extracted.
struct DirectoryMetadata {
    relative: PathBuf,
    uid: u32,
    gid: u32,
    mode: u32,
    mtime: u64,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

fn apply_xattrs(target: &Path, xattrs: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    let path = std::ffi::CString::new(target.as_os_str().as_encoded_bytes())?;
    for (name, value) in xattrs {
        let name = std::ffi::CString::new(name.as_slice())?;
        // SAFETY: valid C strings and buffer; lsetxattr does not follow links.
        if unsafe {
            libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error()).wrap_err("preserve OCI xattr");
        }
    }
    Ok(())
}

fn restore_directory_metadata(tree: &Path, directories: &mut [DirectoryMetadata]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Descendants first: default ACLs must not affect extraction, and final
    // directory modes/times must not be disturbed by children or whiteouts.
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.relative.components().count()));
    for directory in directories {
        ensure_no_symlink_ancestors(tree, &directory.relative)?;
        let path = tree.join(&directory.relative);
        if !std::fs::symlink_metadata(&path)?.is_dir() {
            bail!(
                "OCI directory replaced by non-directory: {}",
                path.display()
            );
        }
        let file = std::fs::File::open(&path)?;
        // SAFETY: live fd and checked numeric ownership; no path following.
        if unsafe { libc::fchown(file.as_raw_fd(), directory.uid, directory.gid) } != 0 {
            return Err(std::io::Error::last_os_error()).wrap_err("preserve directory ownership");
        }
        file.set_permissions(std::fs::Permissions::from_mode(directory.mode))?;
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(directory.mtime)),
        )?;
        apply_xattrs(&path, &directory.xattrs)?;
    }
    Ok(())
}

fn unpack_entry<R: Read>(
    tree: &Path,
    mut entry: tar::Entry<'_, R>,
    directories: &mut Vec<DirectoryMetadata>,
) -> Result<Option<Whiteout>> {
    let raw_path = entry.path()?.to_path_buf();
    let Some(rel) = safe_rel_path(&raw_path) else {
        bail!("refusing to extract suspicious tar path {raw_path:?}");
    };
    let file_name = rel
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    if file_name.starts_with(".wh.") {
        if entry.header().entry_type() != tar::EntryType::Regular || entry.header().size()? != 0 {
            bail!("OCI whiteout {rel:?} must be an empty regular file");
        }
        let parent = rel.parent().unwrap_or(Path::new("")).to_path_buf();
        return Ok(Some(Whiteout { parent, file_name }));
    }

    // All ancestors of the entry must be real directories: earlier layers
    // may have planted symlinks that would otherwise resolve on the host.
    if let Some(parent) = rel.parent() {
        ensure_no_symlink_ancestors(tree, parent)?;
    }
    // A symlink at the entry's own path is replaced, never written through
    // (GNU tar replace semantics: unlink the link first).
    let target = tree.join(&rel);
    if let Ok(meta) = std::fs::symlink_metadata(&target)
        && meta.is_symlink()
    {
        remove_path(&target);
    }

    // Do not let an image inject host OverlayFS control attributes (redirect,
    // metacopy, whiteout, etc.). Only OCI whiteouts may create those markers.
    let mut xattrs = Vec::new();
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions {
            let extension = extension?;
            if let Some(name) = extension.key_bytes().strip_prefix(b"SCHILY.xattr.") {
                if name.starts_with(b"user.overlay.")
                    || !(name.starts_with(b"user.")
                        || name == b"security.capability"
                        || name == b"security.selinux"
                        || name == b"system.posix_acl_access"
                        || name == b"system.posix_acl_default")
                {
                    bail!("unsupported OCI xattr {:?}", String::from_utf8_lossy(name));
                }
                xattrs.push((name.to_vec(), extension.value_bytes().to_vec()));
            }
        }
    }
    match entry.header().entry_type() {
        tar::EntryType::Directory => {
            // tar::unpack_in intentionally ignores headers for '.'; explicit
            // restoration also avoids host ACL inheritance depending on order.
            let metadata = DirectoryMetadata {
                relative: rel.clone(),
                uid: entry.header().uid()?.try_into()?,
                gid: entry.header().gid()?.try_into()?,
                mode: entry.header().mode()?,
                mtime: entry.header().mtime()?,
                xattrs,
            };
            std::fs::create_dir_all(&target)?;
            if let Some(previous) = directories.iter_mut().find(|d| d.relative == rel) {
                *previous = metadata;
            } else {
                directories.push(metadata);
            }
            return Ok(None);
        }
        tar::EntryType::Regular | tar::EntryType::Symlink | tar::EntryType::Link => {
            if entry.header().entry_type() == tar::EntryType::Link {
                let link = entry
                    .link_name()?
                    .ok_or_else(|| eyre!("hard link has no target"))?;
                let relative_target = safe_rel_path(&link)
                    .ok_or_else(|| eyre!("unsafe OCI hard-link target {link:?}"))?;
                ensure_no_symlink_ancestors(tree, &relative_target)?;
                if !tree.join(&relative_target).exists() {
                    bail!(
                        "OCI hard-link target {} is absent from this layer; cross-layer and forward hard links are not supported by digest-only layer materialization",
                        relative_target.display()
                    );
                }
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // unpack_in() extracts at the entry's (already validated) path
            // and handles contents, permissions, symlinks and hard links.
            entry
                .unpack_in(tree)
                .wrap_err_with(|| format!("extract {}", rel.display()))?;
        }
        other
            if rel.starts_with("dev")
                && matches!(
                    other,
                    tar::EntryType::Char | tar::EntryType::Block | tar::EntryType::Fifo
                ) =>
        {
            // The guest kernel's CONFIG_DEVTMPFS_MOUNT supplies /dev. Device
            // nodes/fifos elsewhere are not silently dropped from the root.
            warn!(entry = %rel.display(), ?other, "ignoring device node from image /dev (guest devtmpfs replaces it)");
        }
        other => bail!(
            "unsupported OCI tar entry type {other:?} at {}; refusing to alter image semantics",
            rel.display()
        ),
    }
    apply_xattrs(&target, &xattrs)?;
    Ok(None)
}

fn extract_entries<R: Read>(
    mut archive: tar::Archive<R>,
    tree: &Path,
    max_entries: u64,
) -> Result<(u64, Vec<PathBuf>, Vec<DirectoryReset>)> {
    let mut whiteouts = Vec::new();
    let mut directories = Vec::new();
    let mut count = 0_u64;
    for entry in archive.entries().wrap_err("iterate tar entries")? {
        count = count
            .checked_add(1)
            .ok_or_else(|| eyre!("OCI layer entry count overflows"))?;
        if count > max_entries {
            bail!("OCI layer exceeds the configured {max_entries} entry limit");
        }
        if let Some(whiteout) = unpack_entry(tree, entry.wrap_err("tar entry")?, &mut directories)?
        {
            whiteouts.push(whiteout);
        }
    }
    let opaque_dirs = whiteouts
        .iter()
        .filter(|w| w.file_name == ".wh..wh..opq")
        .map(|w| w.parent.clone())
        .collect::<std::collections::HashSet<_>>();
    let resets = whiteouts
        .iter()
        .map(|w| {
            if w.file_name == ".wh..wh..opq" {
                DirectoryReset {
                    path: w.parent.clone(),
                    descendants_only: true,
                }
            } else {
                DirectoryReset {
                    path: w.parent.join(w.file_name.strip_prefix(".wh.").unwrap()),
                    descendants_only: false,
                }
            }
        })
        .collect();
    for whiteout in whiteouts {
        if whiteout.file_name != ".wh..wh..opq" {
            if opaque_dirs.contains(&whiteout.parent) {
                continue;
            }
            let target_name = whiteout
                .file_name
                .strip_prefix(".wh.")
                .unwrap_or(&whiteout.file_name);
            let target = tree.join(&whiteout.parent).join(target_name);
            // OCI applies whiteouts before adding this layer. A same-layer
            // file therefore takes precedence regardless of tar entry order.
            if std::fs::symlink_metadata(&target).is_ok() {
                continue;
            }
        }
        apply_whiteout(tree, &whiteout.parent, &whiteout.file_name)?;
    }
    restore_directory_metadata(tree, &mut directories)?;
    Ok((
        count,
        directories.into_iter().map(|d| d.relative).collect(),
        resets,
    ))
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct DirectoryReset {
    path: PathBuf,
    descendants_only: bool,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct LayerStats {
    uncompressed_bytes: u64,
    entry_count: u64,
    diff_id: String,
    composefs_image_bytes: u64,
    object_store_bytes: u64,
    /// Only these directories were changed explicitly by the OCI changeset.
    explicit_directories: Vec<PathBuf>,
    directory_resets: Vec<DirectoryReset>,
}

struct LimitedReader<R> {
    inner: R,
    limit: u64,
    bytes: u64,
    hasher: sha2::Sha256,
}

impl<R> LimitedReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            bytes: 0,
            hasher: sha2::Sha256::new(),
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let remaining = self.limit.saturating_sub(self.bytes);
        if remaining == 0 {
            let mut probe = [0_u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(std::io::Error::other(
                    "OCI layer exceeds configured uncompressed-byte limit",
                )),
            };
        }
        let allowed = buf.len().min(remaining.saturating_add(1) as usize);
        let count = self.inner.read(&mut buf[..allowed])?;
        if count as u64 > remaining {
            return Err(std::io::Error::other(
                "OCI layer exceeds configured uncompressed-byte limit",
            ));
        }
        self.bytes += count as u64;
        self.hasher.update(&buf[..count]);
        Ok(count)
    }
}

fn unpack_tar<R: Read>(
    reader: R,
    tree: &Path,
    max_bytes: u64,
    max_entries: u64,
) -> Result<LayerStats> {
    let mut limited = LimitedReader::new(reader, max_bytes);
    let mut archive = tar::Archive::new(&mut limited);
    archive.set_preserve_ownerships(true);
    archive.set_preserve_permissions(true);
    let (entry_count, explicit_directories, directory_resets) =
        extract_entries(archive, tree, max_entries)?;
    // Drain trailing tar padding and any bytes after the end marker so DiffID
    // and the byte bound cover the entire decompressed stream, not only entries.
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if limited.read(&mut buffer)? == 0 {
            break;
        }
    }
    Ok(LayerStats {
        uncompressed_bytes: limited.bytes,
        entry_count,
        diff_id: format!("sha256:{}", to_hex(&limited.hasher.finalize())),
        composefs_image_bytes: 0,
        object_store_bytes: 0,
        explicit_directories,
        directory_resets,
    })
}

/// Unpack a layer whose compressed digest was fully checked by `hash_file`.
/// The bounded decompressor also computes the config's uncompressed DiffID.
fn unpack_layer<R: Read>(
    reader: R,
    compression: Compression,
    tree: &Path,
    max_bytes: u64,
    max_entries: u64,
) -> Result<LayerStats> {
    match compression {
        Compression::None => unpack_tar(reader, tree, max_bytes, max_entries),
        Compression::Gzip => unpack_tar(
            flate2::read::GzDecoder::new(reader),
            tree,
            max_bytes,
            max_entries,
        ),
        Compression::Zstd => unpack_tar(
            zstd::stream::read::Decoder::new(reader).wrap_err("zstd decoder init")?,
            tree,
            max_bytes,
            max_entries,
        ),
    }
}

#[derive(Clone, Debug)]
struct OciLayerDescriptor {
    digest: String,
    diff_id: String,
    media_type: String,
    size: Option<u64>,
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .wrap_err_with(|| format!("open {} for digest verification", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0_u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("sha256:{}", to_hex(&hasher.finalize())))
}

fn normalized_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    }
}

/// Select and integrity-check the platform-specific image manifest, config,
/// and layer descriptors in an OCI layout. This node builds the kernel for its
/// own architecture, so incompatible manifests are rejected rather than used.
fn read_oci_manifest(layout: &Path) -> Result<(String, Vec<OciLayerDescriptor>)> {
    let index_bytes = std::fs::read(layout.join("index.json"))?;
    let index: serde_json::Value =
        serde_json::from_slice(&index_bytes).wrap_err("parse OCI index.json")?;
    let manifests = index["manifests"]
        .as_array()
        .ok_or_else(|| eyre!("OCI index has no manifests"))?;
    let host_arch = normalized_arch(std::env::consts::ARCH);
    let compatible = manifests
        .iter()
        .filter(|m| {
            let platform = &m["platform"];
            platform["os"].as_str() == Some("linux")
                && platform["architecture"]
                    .as_str()
                    .is_some_and(|a| normalized_arch(a) == host_arch)
        })
        .collect::<Vec<_>>();
    let selected = if compatible.len() == 1 {
        compatible[0]
    } else if compatible.is_empty() && manifests.len() == 1 && manifests[0]["platform"].is_null() {
        &manifests[0]
    } else {
        bail!(
            "OCI index has {} manifests compatible with linux/{host_arch}; expected exactly one",
            compatible.len()
        );
    };
    let manifest_digest = selected["digest"]
        .as_str()
        .ok_or_else(|| eyre!("selected OCI descriptor has no digest"))?
        .to_owned();
    let manifest_path = digest_blob_path(layout, &manifest_digest)?;
    if hash_file(&manifest_path)? != manifest_digest {
        bail!("OCI manifest digest mismatch for {manifest_digest}");
    }
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)
        .wrap_err("parse OCI image manifest")?;
    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or_else(|| eyre!("OCI manifest has no config digest"))?;
    let config_path = digest_blob_path(layout, config_digest)?;
    if hash_file(&config_path)? != config_digest {
        bail!("OCI config digest mismatch for {config_digest}");
    }
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config_path)?).wrap_err("parse OCI image config")?;
    let config_os = config["os"].as_str().unwrap_or_default();
    let config_arch = config["architecture"].as_str().unwrap_or_default();
    if config_os != "linux" || normalized_arch(config_arch) != host_arch {
        bail!("OCI config platform {config_os}/{config_arch} does not match linux/{host_arch}");
    }
    let layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| eyre!("OCI manifest has no layers"))?;
    let diff_ids = config["rootfs"]["diff_ids"]
        .as_array()
        .ok_or_else(|| eyre!("OCI config has no rootfs.diff_ids"))?;
    if diff_ids.len() != layers.len() {
        bail!(
            "OCI config has {} DiffIDs for {} filesystem layers",
            diff_ids.len(),
            layers.len()
        );
    }
    let layers = layers
        .iter()
        .zip(diff_ids)
        .enumerate()
        .map(|(index, (layer, diff_id))| {
            let diff_id = diff_id
                .as_str()
                .ok_or_else(|| eyre!("OCI config DiffID {index} is not a string"))?
                .to_owned();
            digest_blob_path(Path::new("/"), &diff_id)
                .wrap_err_with(|| format!("invalid OCI DiffID {index}"))?;
            Ok(OciLayerDescriptor {
                digest: layer["digest"]
                    .as_str()
                    .ok_or_else(|| eyre!("OCI layer {index} has no digest"))?
                    .to_owned(),
                diff_id,
                media_type: layer["mediaType"]
                    .as_str()
                    .ok_or_else(|| eyre!("OCI layer {index} has no mediaType"))?
                    .to_owned(),
                size: Some(
                    layer["size"]
                        .as_u64()
                        .ok_or_else(|| eyre!("OCI layer {index} has no valid size"))?,
                ),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((manifest_digest, layers))
}

// Cache format v6 records explicit directory changes for composition, and
// preserves root directory metadata and restores ACLs
// after extraction to prevent order-dependent child inheritance.
// Older entries lost this metadata and must not be reused. The cache stores
// composefs storage-usage statistics alongside each immutable layer. Versioned
// paths avoid replacing older entries while they may still be mounted.
const LAYERS_ROOT: &str = "/var/lib/odorobo/layers/sha256/v8";

async fn ensure_fsverity(path: &Path) -> Result<()> {
    match run_checked("fsverity", ["enable".to_owned()], &[path]).await {
        Ok(_) => Ok(()),
        Err(enable_error) => {
            // mkcomposefs may enable verity as part of digest-store creation;
            // an already-enabled inode returns EEXIST, accepted only if it can
            // be measured successfully.
            match run_checked("fsverity", ["digest".to_owned()], &[path]).await {
                Ok(measurement) if measurement.trim().starts_with("sha256:") => Ok(()),
                _ => {
                    Err(enable_error
                        .wrap_err(format!("fs-verity unavailable for {}", path.display())))
                }
            }
        }
    }
}

/// Materialize one verified layer into its immutable composefs entry. The OCI
/// digest is the cache key, independent of image-reference aliases.
async fn materialize_layer(
    layout: &Path,
    desc: &OciLayerDescriptor,
    max_bytes: u64,
    max_entries: u64,
) -> Result<(PathBuf, LayerStats)> {
    materialize_layer_at_limited(layout, desc, Path::new(LAYERS_ROOT), max_bytes, max_entries).await
}

#[cfg(test)]
async fn materialize_layer_at(
    layout: &Path,
    desc: &OciLayerDescriptor,
    cache_root: &Path,
) -> Result<PathBuf> {
    let limits = OciUnpackLimits::from_env()?;
    materialize_layer_at_limited(
        layout,
        desc,
        cache_root,
        limits.max_layer_bytes,
        limits.max_image_entries,
    )
    .await
    .map(|(path, _)| path)
}

async fn verify_cached_layer(
    root: &Path,
    desc: &OciLayerDescriptor,
    max_bytes: u64,
    max_entries: u64,
) -> Result<LayerStats> {
    let cached_source = tokio::fs::read_to_string(root.join("oci-digest")).await?;
    if cached_source.trim() != desc.digest {
        bail!(
            "cached layer {} has mismatched OCI digest marker",
            root.display()
        );
    }
    let measured = run_checked(
        "composefs-info",
        ["measure-file".to_owned()],
        &[root.join("layer.cfs").as_path()],
    )
    .await?;
    let expected = tokio::fs::read_to_string(root.join("layer-digest")).await?;
    if measured.trim() != expected.trim() {
        bail!(
            "cached composefs layer {} failed digest verification",
            root.display()
        );
    }
    let stats: LayerStats =
        serde_json::from_slice(&tokio::fs::read(root.join("layer-stats.json")).await?)
            .wrap_err_with(|| format!("read cached layer stats for {}", root.display()))?;
    if stats.diff_id != desc.diff_id {
        bail!(
            "cached layer {} DiffID disagrees with OCI config",
            root.display()
        );
    }
    if stats.uncompressed_bytes > max_bytes || stats.entry_count > max_entries {
        bail!(
            "cached layer {} exceeds the configured extraction limits",
            root.display()
        );
    }
    Ok(stats)
}

async fn materialize_layer_at_limited(
    layout: &Path,
    desc: &OciLayerDescriptor,
    cache_root: &Path,
    max_bytes: u64,
    max_entries: u64,
) -> Result<(PathBuf, LayerStats)> {
    let layout = layout.to_owned();
    let desc = desc.clone();
    let cache_root = cache_root.to_owned();
    // Builder and external writers share one lifetime, including staging
    // cleanup, even when the actor's awaiting future is cancelled.
    tokio::spawn(async move {
        materialize_layer_inner(&layout, &desc, &cache_root, max_bytes, max_entries).await
    })
    .await
    .map_err(|err| eyre!("layer builder task failed: {err}"))?
}

async fn materialize_layer_inner(
    layout: &Path,
    desc: &OciLayerDescriptor,
    cache_root: &Path,
    max_bytes: u64,
    max_entries: u64,
) -> Result<(PathBuf, LayerStats)> {
    let compression = compression_of_media_type(&desc.media_type)
        .ok_or_else(|| eyre!("unsupported OCI layer media type {:?}", desc.media_type))?;
    let blob = digest_blob_path(layout, &desc.digest)?;
    let actual = hash_file(&blob)?;
    if actual != desc.digest {
        bail!(
            "layer digest mismatch: expected {}, got {actual}",
            desc.digest
        );
    }
    if std::fs::metadata(&blob)?.len()
        != desc
            .size
            .ok_or_else(|| eyre!("OCI layer is missing its size"))?
    {
        bail!("layer {} size does not match descriptor", desc.digest);
    }
    let (_, hex) = desc
        .digest
        .split_once(':')
        .ok_or_else(|| eyre!("invalid layer digest"))?;
    let root = cache_root.join(hex);
    if root.join("layer.cfs").is_file()
        && root.join("store").is_dir()
        && root.join("layer-stats.json").is_file()
    {
        let stats = verify_cached_layer(&root, desc, max_bytes, max_entries).await?;
        return Ok((root, stats));
    }
    tokio::fs::create_dir_all(cache_root).await?;
    let tmp = cache_root.join(format!(".layer-{}", ulid::Ulid::generate()));
    let tree = tmp.join("tree");
    // Create staging privately from the outset; the blocking writer owns
    // cleanup until it explicitly hands the finished tree back to the builder.
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(&tmp)?;
    std::fs::create_dir(&tree)?;
    let cleanup = RemovePathOnDrop::new(tmp.clone());
    let blob_for_unpack = blob.clone();
    let tree_clone = tree.clone();
    let diff_id = desc.diff_id.clone();
    let extraction =
        tokio::task::spawn_blocking(move || -> Result<(LayerStats, RemovePathOnDrop)> {
            let cleanup = cleanup;
            let f = std::fs::File::open(&blob_for_unpack)?;
            let stats = unpack_layer(
                std::io::BufReader::new(f),
                compression,
                &tree_clone,
                max_bytes,
                max_entries,
            )?;
            if stats.diff_id != diff_id {
                bail!(
                    "layer DiffID mismatch: expected {diff_id}, uncompressed content is {}",
                    stats.diff_id
                );
            }
            Ok((stats, cleanup))
        })
        .await
        .map_err(|e| eyre!("layer extraction task failed: {e}"))?;
    let (mut stats, _cleanup) = match extraction {
        Ok(result) => result,
        Err(err) => {
            _ = tokio::fs::remove_dir_all(&tmp).await;
            return Err(err.wrap_err("extract bounded OCI layer"));
        }
    };
    let store = tmp.join("store");
    tokio::fs::create_dir_all(&store).await?;
    let image = tmp.join("layer.cfs");
    let mut object_store_bytes = 0_u64;
    let built: Result<()> = async {
        run_checked(
            "mkcomposefs",
            [
                format!("--digest-store={}", store.display()),
                tree.display().to_string(),
                image.display().to_string(),
            ],
            &[],
        )
        .await?;
        ensure_fsverity(&image).await?;
        let verity_digest = run_checked(
            "composefs-info",
            ["measure-file".to_owned()],
            &[image.as_path()],
        )
        .await?;
        tokio::fs::write(tmp.join("layer-digest"), verity_digest.trim()).await?;
        // Seal all backing payloads as well as the composefs metadata image.
        let mut pending = vec![store.clone()];
        while let Some(dir) = pending.pop() {
            let mut entries = tokio::fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let ty = entry.file_type().await?;
                if ty.is_dir() {
                    pending.push(entry.path());
                } else if ty.is_file() {
                    ensure_fsverity(&entry.path()).await?;
                    object_store_bytes = object_store_bytes
                        .checked_add(entry.metadata().await?.len())
                        .ok_or_else(|| eyre!("composefs object store size overflows"))?;
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(err) = built {
        _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(err.wrap_err("build fs-verity-protected composefs layer"));
    }
    stats.composefs_image_bytes = tokio::fs::metadata(&image).await?.len();
    stats.object_store_bytes = object_store_bytes;
    if let Err(err) = tokio::fs::remove_dir_all(&tree).await {
        _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(err.into());
    }
    tokio::fs::write(tmp.join("oci-digest"), format!("{}\n", desc.digest)).await?;
    tokio::fs::write(tmp.join("layer-stats.json"), serde_json::to_vec(&stats)?).await?;
    match tokio::fs::rename(&tmp, &root).await {
        Ok(()) => Ok((root, stats)),
        Err(_err) if root.exists() => {
            _ = tokio::fs::remove_dir_all(&tmp).await;
            let winner_stats = verify_cached_layer(&root, desc, max_bytes, max_entries).await?;
            Ok((root, winner_stats))
        }
        Err(err) => {
            _ = tokio::fs::remove_dir_all(&tmp).await;
            Err(eyre!("publish layer cache {}: {err}", root.display()))
        }
    }
}

// ---------------------------------------------------------------------------
// composefs layer cache + overlay mount
// ---------------------------------------------------------------------------

/// vhost-user socket path for a VM's rootfs device. Single source of truth
/// shared with the manifest conversion's FsConfig so the actor's virtiofsd
/// supervisor and the CH fs device config always agree.
pub fn socket_path_for(vmid: &str) -> PathBuf {
    crate::ch_driver::VMInstance::runtime_dir_for(vmid).join(format!("{ROOTFS_TAG}.sock"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PersistentBackend {
    Local,
    Rbd,
}

fn persistent_backend() -> Result<PersistentBackend> {
    match std::env::var("ODOROBO_ROOTFS_BACKEND")
        .unwrap_or_else(|_| "local".to_owned())
        .as_str()
    {
        "local" => Ok(PersistentBackend::Local),
        "rbd" => Ok(PersistentBackend::Rbd),
        other => bail!("ODOROBO_ROOTFS_BACKEND must be 'local' or 'rbd', not {other:?}"),
    }
}

fn rbd_prefix_args() -> Vec<String> {
    let mut args = Vec::new();
    for (env, flag) in [
        ("CEPH_CONFIG", "--conf"),
        ("CEPH_ID", "--id"),
        ("CEPH_KEYFILE", "--keyfile"),
        ("CEPH_CLUSTER", "--cluster"),
    ] {
        if let Ok(value) = std::env::var(env) {
            args.push(format!("{flag}={value}"));
        }
    }
    args
}

fn rbd_pool() -> String {
    std::env::var("ODOROBO_ROOTFS_RBD_POOL")
        .or_else(|_| std::env::var("CEPH_POOL"))
        .unwrap_or_else(|_| "odorobo-blockpool".to_owned())
}

async fn is_mountpoint(path: &Path) -> Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
        Ok(_) => {}
    }
    let output = Command::new("mountpoint")
        .arg("-q")
        .arg(path)
        .output()
        .await
        .wrap_err("mountpoint command is required to protect rootfs state lifecycle")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(32) => Ok(false),
        _ => bail!(
            "mountpoint check failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

async fn rbd_lookup_device(image: &str) -> Result<Option<PathBuf>> {
    let output = rbd_output(&[
        "device".into(),
        "list".into(),
        "--format".into(),
        "json".into(),
    ])
    .await?;
    if !output.status.success() {
        bail!(
            "list RBD devices: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let devices: serde_json::Value =
        serde_json::from_slice(&output.stdout).wrap_err("parse RBD device list")?;
    match_rbd_head_device(image, &devices)
}

fn match_rbd_head_device(image: &str, devices: &serde_json::Value) -> Result<Option<PathBuf>> {
    let pool = image.split('/').next().unwrap_or_default();
    let name = image.split('/').nth(1).unwrap_or_default();
    for mapping in devices
        .as_array()
        .ok_or_else(|| eyre!("RBD device list is not an array"))?
    {
        if mapping["pool"].as_str() == Some(pool)
            && mapping["namespace"].as_str().unwrap_or_default().is_empty()
            && matches!(mapping["snap"].as_str().unwrap_or_default(), "" | "-")
            && matches!(mapping["snapshot"].as_str().unwrap_or_default(), "" | "-")
            && (mapping["name"].as_str() == Some(name) || mapping["image"].as_str() == Some(name))
        {
            let device = mapping["device"]
                .as_str()
                .ok_or_else(|| eyre!("RBD mapping has no device path"))?;
            if !device.starts_with("/dev/") {
                bail!("unsafe RBD device path {device:?}");
            }
            return Ok(Some(PathBuf::from(device)));
        }
    }
    Ok(None)
}

fn rbd_size() -> Result<String> {
    let size = std::env::var("ODOROBO_ROOTFS_RBD_SIZE").unwrap_or_else(|_| "1G".to_owned());
    parse_rbd_size(&size)
}

fn parse_rbd_size(size: &str) -> Result<String> {
    let size = size.trim();
    let (digits, suffix) = size.split_at(
        size.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(size.len()),
    );
    let n: u64 = digits
        .parse()
        .map_err(|_| eyre!("invalid ODOROBO_ROOTFS_RBD_SIZE {size:?}"))?;
    let multiplier = match suffix.to_ascii_uppercase().as_str() {
        "M" => 1,
        "G" => 1024,
        "T" => 1024 * 1024,
        _ => bail!("RBD size must use M, G, or T units"),
    };
    let mib = n
        .checked_mul(multiplier)
        .ok_or_else(|| eyre!("RBD size overflows"))?;
    if !(64..=65536).contains(&mib) {
        bail!("RBD persistent upper size must be between 64M and 64G");
    }
    Ok(size.to_owned())
}

fn validate_vmid(vmid: &str) -> Result<()> {
    if vmid.is_empty() || !vmid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("unsafe VM id for rootfs state path");
    }
    Ok(())
}

fn rbd_identity(vmid: &str) -> Result<String> {
    validate_vmid(vmid)?;
    Ok(format!("{}/odorobo-rootfs-{vmid}", rbd_pool()))
}

async fn acquire_rootfs_lock(vmid: &str) -> Result<Arc<std::fs::File>> {
    validate_vmid(vmid)?;
    let dir = PathBuf::from("/run/odorobo/rootfs-locks");
    tokio::fs::create_dir_all(&dir).await?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(dir.join(format!("{vmid}.lock")))?;
    // SAFETY: flock operates on a live file descriptor owned by `file`.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error())
            .wrap_err_with(|| format!("rootfs state for VM {vmid} is already owned on this node"));
    }
    Ok(Arc::new(file))
}

static TOOL_OPERATIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Keep command ownership outside the caller's cancellable future. Cleanup
/// takes the same operation lock, so it cannot inspect/unmap before a cancelled
/// acquisition has completed (or timed out, been killed and reaped).
async fn rbd_output(args: &[String]) -> Result<std::process::Output> {
    let mut command = Command::new("rbd");
    command.args(rbd_prefix_args()).args(args);
    run_owned_command(command).await
}

async fn run_owned_command(mut command: Command) -> Result<std::process::Output> {
    let guard = TOOL_OPERATIONS.lock().await;
    command
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command.as_std_mut().process_group(0);
    let child = command
        .spawn()
        .wrap_err("failed to execute external tool")?;
    let task = tokio::spawn(async move {
        let _guard = guard;
        let pid = child
            .id()
            .ok_or_else(|| eyre!("external tool child has no PID"))?;
        let output = child.wait_with_output();
        tokio::pin!(output);
        match tokio::time::timeout(Duration::from_secs(120), &mut output).await {
            Ok(result) => result.wrap_err("external tool operation failed"),
            Err(_) => {
                // SAFETY: child is still owned by the pinned wait future and
                // unreaped; its PID cannot be reused before that future ends.
                unsafe {
                    libc::kill(-i32::try_from(pid)?, libc::SIGKILL);
                }
                _ = output.await;
                bail!("external tool timed out; process group killed and child reaped");
            }
        }
    });
    task.await
        .map_err(|err| eyre!("external tool task failed: {err}"))?
}

async fn rbd_image_exists(image: &str) -> Result<bool> {
    let output = rbd_output(&[
        "info".into(),
        "--format".into(),
        "json".into(),
        image.into(),
    ])
    .await?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such file or directory") || stderr.contains("does not exist") {
        Ok(false)
    } else {
        bail!("inspect RBD image {image}: {}", stderr.trim())
    }
}

pub(crate) static PERSISTENT_OPERATIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
pub(crate) static RBD_RESERVATIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(Default::default);
// None = command started but device not yet established; absence of a key =
// never acquired (including explicit preflight rejection).
static RBD_STAGING_IMAGES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
> = std::sync::LazyLock::new(Default::default);

async fn cleanup_staging_images(image: &str) -> Result<()> {
    loop {
        let staging = RBD_STAGING_IMAGES
            .lock()
            .unwrap()
            .get(image)
            .and_then(|images| images.last())
            .cloned();
        let Some(staging) = staging else {
            return Ok(());
        };
        if rbd_image_exists(&staging).await? {
            let removed = rbd_output(&["rm".into(), staging.clone()]).await?;
            if !removed.status.success() {
                bail!("remove retained staging RBD image {staging} failed");
            }
        }
        RBD_STAGING_IMAGES
            .lock()
            .unwrap()
            .get_mut(image)
            .unwrap()
            .pop();
    }
}

static PERSISTENT_RBD_OWNERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, Option<PathBuf>>>,
> = std::sync::LazyLock::new(Default::default);

async fn mount_persistent_state(vmid: &str) -> Result<(PathBuf, Option<String>)> {
    let guard = PERSISTENT_OPERATIONS.lock().await;
    let vmid = vmid.to_owned();
    tokio::spawn(async move {
        let _guard = guard;
        mount_persistent_state_inner(&vmid).await
    })
    .await
    .map_err(|err| eyre!("persistent attachment task failed: {err}"))?
}

async fn mount_persistent_state_inner(vmid: &str) -> Result<(PathBuf, Option<String>)> {
    let path = PathBuf::from(PERSISTENT_ROOTFS_ROOT).join(vmid);
    tokio::fs::create_dir_all(&path).await?;
    if is_mountpoint(&path).await? {
        bail!(
            "persistent state path {} is already mounted",
            path.display()
        );
    }
    if persistent_backend()? == PersistentBackend::Local {
        return Ok((path, None));
    }
    let pool = rbd_pool();
    if pool.is_empty()
        || !pool
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        bail!("invalid RBD pool name {pool:?}");
    }
    let image = rbd_identity(vmid)?;
    cleanup_staging_images(&image).await?;
    let created = if rbd_image_exists(&image).await? {
        false
    } else {
        // Publish the final VM identity only after its provisioning phase is
        // durable in RBD metadata. A crash before publication cannot turn a
        // blank image into an apparently established VM volume.
        let staging_image = format!("{image}-initializing-{}", ulid::Ulid::generate());
        // Record before the command: timeout/transport failure can still leave
        // the created volume. Reconciliation checks existence before removal.
        RBD_STAGING_IMAGES
            .lock()
            .unwrap()
            .entry(image.clone())
            .or_default()
            .push(staging_image.clone());
        let create = rbd_output(&[
            "create".into(),
            "--size".into(),
            rbd_size()?,
            "--image-feature".into(),
            "exclusive-lock".into(),
            staging_image.clone(),
        ])
        .await?;
        if !create.status.success() {
            bail!(
                "create RBD persistent rootfs image {image}: {}",
                String::from_utf8_lossy(&create.stderr).trim()
            );
        }
        // A volume may exist before its first mapping/formatting succeeds.
        // Keep this provisioning phase on the volume, not node-local state.
        let phase = rbd_output(&[
            "image-meta".into(),
            "set".into(),
            staging_image.clone(),
            "odorobo.rootfs.phase".into(),
            "uninitialized".into(),
        ])
        .await?;
        if !phase.status.success() {
            let removed = rbd_output(&["rm".into(), staging_image.clone()]).await;
            bail!("record new RBD provisioning phase failed; rollback: {removed:?}");
        }
        let published =
            rbd_output(&["rename".into(), staging_image.clone(), image.clone()]).await?;
        if !published.status.success() {
            let removed = cleanup_staging_images(&image).await;
            bail!("publish RBD image failed; staging cleanup: {removed:?}");
        }
        RBD_STAGING_IMAGES
            .lock()
            .unwrap()
            .get_mut(&image)
            .unwrap()
            .pop();
        true
    };
    let info_args = vec![
        "info".into(),
        "--format".into(),
        "json".into(),
        image.clone(),
    ];
    let info = rbd_output(&info_args).await?;
    if !info.status.success() {
        bail!(
            "inspect RBD persistent rootfs image {image}: {}",
            String::from_utf8_lossy(&info.stderr).trim()
        );
    }
    let info_json: serde_json::Value =
        serde_json::from_slice(&info.stdout).wrap_err("parse RBD image info")?;
    let has_exclusive_lock = info_json["features"]
        .as_str()
        .is_some_and(|features| features.contains("exclusive-lock"))
        || info_json["features"].as_array().is_some_and(|features| {
            features
                .iter()
                .any(|feature| feature.as_str() == Some("exclusive-lock"))
        });
    if !has_exclusive_lock {
        bail!(
            "RBD rootfs image {image} lacks exclusive-lock; refusing unsafe multi-writer attachment"
        );
    }
    // Never adopt a mapping whose ownership/lock mode we cannot establish.
    if RBD_RESERVATIONS.lock().unwrap().contains(&image)
        || rbd_lookup_device(&image).await?.is_some()
    {
        bail!(
            "RBD image {image} is mapped or has unresolved ownership; detach it before restarting"
        );
    }
    RBD_RESERVATIONS.lock().unwrap().insert(image.clone());
    PERSISTENT_RBD_OWNERS
        .lock()
        .unwrap()
        .insert(image.clone(), None);
    let device = {
        let map = rbd_output(&[
            "device".into(),
            "map".into(),
            "--exclusive".into(),
            "--options".into(),
            "noudev,lock_timeout=30".into(),
            image.clone(),
        ])
        .await?;
        if !map.status.success() {
            bail!(
                "map RBD rootfs image {image} (possibly fenced on another node): {}",
                String::from_utf8_lossy(&map.stderr).trim()
            );
        }
        let device = String::from_utf8_lossy(&map.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if !device.starts_with("/dev/") {
            bail!("rbd map returned invalid device path {device:?}");
        }
        PathBuf::from(device)
    };
    PERSISTENT_RBD_OWNERS
        .lock()
        .unwrap()
        .insert(image.clone(), Some(device.clone()));
    let attach = async {
        if is_device_mounted(&device).await? {
            bail!(
                "RBD device {} is mounted elsewhere on this node",
                device.display()
            );
        }
        let phase = rbd_output(&[
            "image-meta".into(),
            "get".into(),
            image.clone(),
            "odorobo.rootfs.phase".into(),
        ])
        .await?;
        let phase_text = String::from_utf8_lossy(&phase.stdout);
        let initialize = if phase.status.success() {
            match phase_text.trim() {
                "uninitialized" => true,
                "formatted" => false,
                other => bail!("unknown RBD provisioning phase {other:?}"),
            }
        } else if created {
            bail!("new RBD image lost its provisioning phase");
        } else {
            // Legacy established volumes lack this key. Never format them.
            false
        };
        if initialize {
            run_checked("mkfs.ext4", ["-F".into()], &[&device]).await?;
            let formatted = rbd_output(&[
                "image-meta".into(),
                "set".into(),
                image.clone(),
                "odorobo.rootfs.phase".into(),
                "formatted".into(),
            ])
            .await?;
            if !formatted.status.success() {
                bail!("record formatted RBD phase failed");
            }
        } else {
            let check = Command::new("e2fsck")
                .arg("-p")
                .arg(&device)
                .output()
                .await?;
            if !matches!(check.status.code(), Some(0 | 1)) {
                bail!(
                    "RBD filesystem check failed ({}): {}",
                    check.status,
                    String::from_utf8_lossy(&check.stderr).trim()
                );
            }
        }
        run_checked(
            "mount",
            [
                "-t".into(),
                "ext4".into(),
                "-o".into(),
                "noatime,nosuid,nodev".into(),
            ],
            &[&device, &path],
        )
        .await?;
        Ok::<_, stable_eyre::Report>(())
    }
    .await;
    if let Err(err) = attach {
        let cleanup = release_persistent_backend_inner(None, Some(&image)).await;
        return Err(err.wrap_err(format!("RBD attachment rollback: {cleanup:?}")));
    }
    Ok((path, Some(image)))
}

async fn is_device_mounted(device: &Path) -> Result<bool> {
    let wanted = std::fs::canonicalize(device).unwrap_or_else(|_| device.to_path_buf());
    let mounts = tokio::fs::read_to_string("/proc/mounts").await?;
    for line in mounts.lines() {
        if let Some(source) = line.split_whitespace().next() {
            let source = PathBuf::from(source);
            let source = std::fs::canonicalize(&source).unwrap_or(source);
            if source == wanted {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

struct UpperState {
    upper: Option<PathBuf>,
    work: Option<PathBuf>,
    state_mount: Option<PathBuf>,
    rbd_image: Option<String>,
}

async fn prepare_upper(
    vmid: &str,
    mode: RootfsMode,
    manifest_digest: &str,
    runtime_dir: &Path,
) -> Result<UpperState> {
    if mode == RootfsMode::ReadOnly {
        return Ok(UpperState {
            upper: None,
            work: None,
            state_mount: None,
            rbd_image: None,
        });
    }
    let mut rbd_image = None;
    let state_dir = if mode == RootfsMode::Persistent {
        let (path, image) = mount_persistent_state(vmid).await?;
        rbd_image = image;
        path
    } else {
        runtime_dir.to_path_buf()
    };
    let result: Result<(PathBuf, PathBuf)> = async {
        let upper = state_dir.join("rootfs.upper");
        let work = state_dir.join("rootfs.work");
        if mode == RootfsMode::Ephemeral {
            _ = tokio::fs::remove_dir_all(&upper).await;
            _ = tokio::fs::remove_dir_all(&work).await;
        }
        tokio::fs::create_dir_all(&upper).await?;
        if mode == RootfsMode::Persistent {
            let metadata = state_dir.join("rootfs-state.json");
            let backend = if rbd_image.is_some() { "rbd" } else { "local" };
            let expected = serde_json::json!({"version":1,"vmid":vmid,"base_manifest_digest":manifest_digest,"overlay":"overlayfs-v1","backend":backend});
            if metadata.exists() {
                let existing: serde_json::Value = serde_json::from_slice(&tokio::fs::read(&metadata).await?)
                    .wrap_err("invalid persistent rootfs state metadata")?;
                if existing != expected { bail!("persistent rootfs state for VM {vmid} is pinned to a different base/backend/format; refusing reuse"); }
            } else {
                let mut entries = tokio::fs::read_dir(&upper).await?;
                if entries.next_entry().await?.is_some() { bail!("persistent upper for VM {vmid} lacks pin metadata; refusing unsafe adoption"); }
                let tmp = state_dir.join(format!(".rootfs-state-{}.tmp", ulid::Ulid::generate()));
                tokio::fs::write(&tmp, serde_json::to_vec_pretty(&expected)?).await?;
                tokio::fs::rename(&tmp, &metadata).await?;
            }
        }
        _ = tokio::fs::remove_dir_all(&work).await;
        tokio::fs::create_dir(&work).await?;
        Ok((upper, work))
    }.await;
    let (upper, work) = match result {
        Ok(paths) => paths,
        Err(err) => {
            if let Some(image) = &rbd_image {
                if let Err(cleanup) =
                    release_persistent_backend(Some(&state_dir), Some(image)).await
                {
                    return Err(err.wrap_err(format!(
                        "persistent rootfs preparation failed; rollback failed: {cleanup}"
                    )));
                }
            }
            return Err(err);
        }
    };
    Ok(UpperState {
        upper: Some(upper),
        work: Some(work),
        state_mount: rbd_image.as_ref().map(|_| state_dir),
        rbd_image,
    })
}

async fn release_persistent_backend(
    state_mount: Option<&Path>,
    rbd_image: Option<&str>,
) -> Result<()> {
    let _guard = PERSISTENT_OPERATIONS.lock().await;
    release_persistent_backend_inner(state_mount, rbd_image).await
}

async fn release_persistent_backend_inner(
    state_mount: Option<&Path>,
    rbd_image: Option<&str>,
) -> Result<()> {
    if let Some(path) = state_mount {
        unmount_path(path).await?;
    }
    if let Some(image) = rbd_image {
        cleanup_staging_images(image).await?;
        let ownership = PERSISTENT_RBD_OWNERS.lock().unwrap().get(image).cloned();
        let Some(expected) = ownership else {
            return Ok(());
        };
        let Some(device) = rbd_lookup_device(image).await? else {
            PERSISTENT_RBD_OWNERS.lock().unwrap().remove(image);
            RBD_RESERVATIONS.lock().unwrap().remove(image);
            return Ok(());
        };
        if expected
            .as_ref()
            .is_some_and(|expected| *expected != device)
        {
            bail!("persistent RBD mapping identity changed; refusing release");
        }
        if is_device_mounted(&device).await? {
            bail!("refusing to unmap mounted RBD device {}", device.display());
        }
        let output = rbd_output(&[
            "device".into(),
            "unmap".into(),
            "--options".into(),
            "noudev".into(),
            image.to_owned(),
        ])
        .await?;
        if !output.status.success() {
            bail!(
                "unmap RBD rootfs {image}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        PERSISTENT_RBD_OWNERS.lock().unwrap().remove(image);
        RBD_RESERVATIONS.lock().unwrap().remove(image);
    }
    Ok(())
}

/// Idempotence is essential: teardown may have succeeded only partially on
/// its previous attempt. Never treat an already-detached mount as a failure.
async fn unmount_path(path: &Path) -> Result<()> {
    // An acquisition may outlive a cancelled caller; observe its completion
    // before checking whether the target needs detaching.
    {
        let _barrier = TOOL_OPERATIONS.lock().await;
    }
    if is_mountpoint(path).await? {
        run_checked("umount", [], &[path]).await?;
    }
    Ok(())
}

async fn unmount_layer_mounts(mounts: &[PathBuf]) -> Result<()> {
    for path in mounts.iter().rev() {
        unmount_path(path).await?;
    }
    Ok(())
}

/// Prepare a digest-addressed composefs mount for each OCI layer, then expose
/// their OCI-ordered OverlayFS stack as the VM root. No flattened image copy is
/// retained; only per-layer stores and per-reference OCI layouts are cached.
#[cfg(test)]
async fn mount_rootfs(vmid: &str, rootfs: &Rootfs) -> Result<PreparedRootfs> {
    let mut owner = None;
    match mount_rootfs_owned(vmid, rootfs, &mut owner).await {
        Ok(()) => Ok(owner.unwrap()),
        Err(err) => {
            if let Some(prepared) = owner.as_ref() {
                unmount_rootfs(prepared).await.wrap_err("rootfs rollback")?;
            }
            Err(err)
        }
    }
}

/// Publish acquired resources to a lifecycle owner before the first mount.
/// A failed startup retains this inventory for an explicit teardown retry.
pub async fn mount_rootfs_owned(
    vmid: &str,
    rootfs: &Rootfs,
    owner: &mut Option<PreparedRootfs>,
) -> Result<()> {
    validate_vmid(vmid)?;
    let limits = OciUnpackLimits::from_env()?;
    let state_lock = acquire_rootfs_lock(vmid).await?;
    let layout = pull_oci(&rootfs.oci, limits.max_compressed_bytes).await?;
    let layout_for_parse = layout.clone();
    let (manifest_digest, descriptors) =
        tokio::task::spawn_blocking(move || read_oci_manifest(&layout_for_parse))
            .await
            .map_err(|e| eyre!("OCI manifest validation task failed: {e}"))??;
    if descriptors.is_empty() {
        bail!("OCI image has no filesystem layers");
    }
    if descriptors.len() > 500 {
        bail!(
            "OCI image has {} layers; OverlayFS supports at most 500 lower layers",
            descriptors.len()
        );
    }
    let mut seen_layer_digests = std::collections::HashSet::new();
    let compressed_bytes = descriptors.iter().try_fold(0_u64, |sum, desc| {
        if !seen_layer_digests.insert(&desc.digest) {
            return Ok(sum);
        }
        sum.checked_add(desc.size.ok_or_else(|| eyre!("OCI layer has no size"))?)
            .ok_or_else(|| eyre!("total compressed OCI size overflows"))
    })?;
    if compressed_bytes > limits.max_compressed_bytes {
        bail!(
            "OCI image compressed layers total {compressed_bytes} bytes, exceeding configured limit {}",
            limits.max_compressed_bytes
        );
    }

    let runtime_dir = crate::ch_driver::VMInstance::runtime_dir_for(vmid);
    tokio::fs::create_dir_all(&runtime_dir).await?;
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700)).await?;
    let root_mount = runtime_dir.join(ROOTFS_TAG);
    tokio::fs::create_dir_all(&root_mount).await?;
    if is_mountpoint(&root_mount).await? {
        bail!(
            "rootfs mountpoint {} is already mounted",
            root_mount.display()
        );
    }
    // Materialize every cache entry before mounting anything, so extraction or
    // integrity failures cannot strand a partial stack.
    let mut caches = Vec::with_capacity(descriptors.len());
    let mut directory_changes = Vec::with_capacity(descriptors.len());
    let mut directory_resets = Vec::with_capacity(descriptors.len());
    let mut unpacked_bytes = 0_u64;
    let mut unpacked_entries = 0_u64;
    let mut composefs_image_bytes = 0_u64;
    let mut composefs_object_bytes = 0_u64;
    let mut accounted_layers = std::collections::HashSet::new();
    for desc in &descriptors {
        let remaining_bytes = limits.max_image_bytes.saturating_sub(unpacked_bytes);
        let remaining_entries = limits.max_image_entries.saturating_sub(unpacked_entries);
        if remaining_bytes == 0 || remaining_entries == 0 {
            bail!("OCI image exceeds configured aggregate extraction limits");
        }
        let layer_limit = limits.max_layer_bytes.min(remaining_bytes);
        let (cache, stats) =
            materialize_layer(&layout, desc, layer_limit, remaining_entries).await?;
        unpacked_bytes = unpacked_bytes
            .checked_add(stats.uncompressed_bytes)
            .ok_or_else(|| eyre!("unpacked image size overflows"))?;
        unpacked_entries = unpacked_entries
            .checked_add(stats.entry_count)
            .ok_or_else(|| eyre!("unpacked image entry count overflows"))?;
        if accounted_layers.insert(&desc.digest) {
            composefs_image_bytes = composefs_image_bytes
                .checked_add(stats.composefs_image_bytes)
                .ok_or_else(|| eyre!("composefs image cache size overflows"))?;
            composefs_object_bytes = composefs_object_bytes
                .checked_add(stats.object_store_bytes)
                .ok_or_else(|| eyre!("composefs object cache size overflows"))?;
        }
        directory_changes.push(stats.explicit_directories);
        directory_resets.push(stats.directory_resets);
        caches.push(cache);
    }
    let layer_root = runtime_dir.join("rootfs-layers");
    tokio::fs::create_dir_all(&layer_root).await?;
    let mut mount_specs = Vec::with_capacity(descriptors.len());
    for (index, (desc, cache)) in descriptors.iter().zip(&caches).enumerate() {
        let (_, hex) = desc.digest.split_once(':').expect("validated layer digest");
        let layer_mount = layer_root.join(format!("{index}-{hex}"));
        tokio::fs::create_dir_all(&layer_mount).await?;
        if is_mountpoint(&layer_mount).await? {
            bail!(
                "OCI layer mountpoint {} is already mounted",
                layer_mount.display()
            );
        }
        let verity_digest = tokio::fs::read_to_string(cache.join("layer-digest")).await?;
        mount_specs.push((
            desc.digest.clone(),
            cache.clone(),
            verity_digest.trim().to_owned(),
            layer_mount,
        ));
    }
    *owner = Some(PreparedRootfs {
        digest: manifest_digest.clone(),
        mount: root_mount.clone(),
        upper: None,
        work: None,
        layer_mounts: mount_specs
            .iter()
            .map(|(_, _, _, path)| path.clone())
            .collect(),
        persistent_state_mount: None,
        rbd_image: None,
        _state_lock: state_lock,
        mode: rootfs.mode,
        scratch_mounts: Vec::new(),
    });
    let mut lowerdirs = Vec::with_capacity(mount_specs.len());
    for (_layer_digest, cache, verity_digest, layer_mount) in &mount_specs {
        let opts = format!(
            "basedir={},digest={},verity,ro",
            cache.join("store").display(),
            verity_digest
        );
        run_checked(
            "mount.composefs",
            ["-o".to_owned(), opts],
            &[cache.join("layer.cfs").as_path(), layer_mount.as_path()],
        )
        .await?;
        run_checked(
            "mount",
            ["--make-rprivate".to_owned()],
            &[layer_mount.as_path()],
        )
        .await?;
        // mount.composefs does not accept standard nosuid/nodev options;
        // apply these host-side restrictions to the resulting mount instead.
        run_checked(
            "mount",
            ["-o".into(), "remount,bind,ro,nosuid,nodev".into()],
            &[layer_mount],
        )
        .await?;
        lowerdirs.push(layer_mount.clone());
    }
    // Correct directory-only metadata in a VM-private composefs layer. OCI
    // omitted ancestors inherit prior metadata, unlike OverlayFS's highest
    // directory rule. Payload caches remain immutable and fully reusable.
    let correction = runtime_dir.join("rootfs-directory-metadata");
    owner
        .as_mut()
        .unwrap()
        .layer_mounts
        .push(correction.clone());
    let corrected = compose_directory_metadata(
        &runtime_dir,
        &root_mount,
        &lowerdirs,
        &directory_changes,
        &directory_resets,
    )
    .await?;
    // OCI order is base to top; OverlayFS expects its highest-priority lower first.
    lowerdirs.reverse();
    lowerdirs.insert(0, corrected);

    // Include persistence in inventory before attaching; failed formatting,
    // metadata validation or unmapping must remain reachable for cleanup.
    if rootfs.mode == RootfsMode::Persistent && persistent_backend()? == PersistentBackend::Rbd {
        let image = rbd_identity(vmid)?;
        let state_path = Path::new(PERSISTENT_ROOTFS_ROOT).join(vmid);
        if rbd_lookup_device(&image).await?.is_some() || is_mountpoint(&state_path).await? {
            bail!("persistent RBD state already attached; refusing ownership adoption");
        }
        let prepared = owner.as_mut().unwrap();
        prepared.persistent_state_mount = Some(state_path);
        prepared.rbd_image = Some(image);
    }
    let upper_state = prepare_upper(vmid, rootfs.mode, &manifest_digest, &runtime_dir).await?;
    {
        let prepared = owner.as_mut().unwrap();
        prepared.upper = upper_state.upper.clone();
        prepared.work = upper_state.work.clone();
    }
    if rootfs.mode == RootfsMode::ReadOnly && lowerdirs.len() == 1 {
        // A second, controlled empty lower preserves OverlayFS interpretation
        // of OCI whiteouts. A direct bind export would expose marker files.
        let empty = runtime_dir.join("rootfs-empty-lower");
        tokio::fs::create_dir_all(&empty).await?;
        lowerdirs.push(empty);
    }
    let mut options = format!(
        "nosuid,nodev,lowerdir={}",
        lowerdirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    );
    if let (Some(upper), Some(work)) = (&upper_state.upper, &upper_state.work) {
        options.push_str(&format!(
            ",upperdir={},workdir={}",
            upper.display(),
            work.display()
        ));
    } else {
        options.push_str(",ro");
    }
    if options.len() >= 4096 {
        bail!("OCI OverlayFS mount options exceed the kernel page-size limit");
    }
    run_checked(
        "mount",
        [
            "-t".into(),
            "overlay".into(),
            "overlay".into(),
            "-o".into(),
            options,
        ],
        &[root_mount.as_path()],
    )
    .await?;
    run_checked(
        "mount",
        ["--make-rprivate".to_owned()],
        &[root_mount.as_path()],
    )
    .await?;
    // Record scratch targets before mounting so even failed scratch rollback
    // can be retried through the owner.
    if rootfs.mode == RootfsMode::ReadOnly {
        owner.as_mut().unwrap().scratch_mounts = validate_scratch_targets(&root_mount)?
            .into_iter()
            .map(|(path, _)| path)
            .collect();
    }
    let scratch_mounts = if rootfs.mode == RootfsMode::ReadOnly {
        mount_readonly_scratch(&root_mount).await?
    } else {
        Vec::new()
    };
    info!(vmid, %manifest_digest, layers=descriptors.len(), unique_layer_blob_bytes=compressed_bytes, composefs_image_bytes, composefs_object_bytes, mount=%root_mount.display(), "OCI rootfs mounted; referenced cache usage (compressed blobs / composefs metadata / composefs objects)");
    owner.as_mut().unwrap().scratch_mounts = scratch_mounts;
    Ok(())
}

fn copy_directory_attributes(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::symlink_metadata(source)?;
    if !metadata.is_dir() {
        bail!("directory metadata source is not a directory");
    }
    let file = std::fs::File::open(destination)?;
    // SAFETY: live fd and numeric metadata from verified layer.
    if unsafe { libc::fchown(file.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0 {
        return Err(std::io::Error::last_os_error()).wrap_err("copy directory owner");
    }
    file.set_permissions(std::fs::Permissions::from_mode(metadata.mode()))?;
    file.set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
    let path = std::ffi::CString::new(source.as_os_str().as_encoded_bytes())?;
    // SAFETY: valid path; null buffer queries length.
    let length = unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    if length < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut names = vec![0_u8; length as usize];
    // SAFETY: valid allocated buffer and path.
    let length = unsafe { libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if length < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut attributes = Vec::new();
    for name in names[..length as usize]
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        if name.starts_with(b"trusted.overlay.") || name.starts_with(b"user.overlay.") {
            continue;
        }
        let cname = std::ffi::CString::new(name)?;
        // SAFETY: valid strings, null buffer for length query.
        let size =
            unsafe { libc::lgetxattr(path.as_ptr(), cname.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut value = vec![0_u8; size as usize];
        // SAFETY: valid output buffer sized by preceding query.
        if unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                cname.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        attributes.push((name.to_vec(), value));
    }
    apply_xattrs(destination, &attributes)
}

fn directory_metadata_source(
    relative: &Path,
    layers: &[PathBuf],
    changes: &[Vec<PathBuf>],
    resets: &[Vec<DirectoryReset>],
) -> Result<usize> {
    let mut source = None;
    for (index, layer) in layers.iter().enumerate() {
        if resets[index].iter().any(|reset| {
            relative.starts_with(&reset.path) && (!reset.descendants_only || relative != reset.path)
        }) {
            source = None;
        }
        // Examine every component with lstat; never traverse image symlinks.
        let mut prefix = layer.clone();
        let mut blocked = false;
        for component in relative.components() {
            prefix.push(component);
            match std::fs::symlink_metadata(&prefix) {
                Ok(metadata) if !metadata.is_dir() => {
                    blocked = true;
                    break;
                }
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    break;
                }
                Err(err) => return Err(err.into()),
            }
        }
        if blocked {
            source = None;
            continue;
        }
        if std::fs::symlink_metadata(layer.join(relative)).is_ok_and(|m| m.is_dir())
            && (source.is_none() || changes[index].iter().any(|path| path == relative))
        {
            source = Some(index);
        }
    }
    source.ok_or_else(|| {
        eyre!(
            "no current-generation metadata source for {}",
            relative.display()
        )
    })
}

static DIRECTORY_OPERATIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn compose_directory_metadata(
    runtime: &Path,
    temporary_mount: &Path,
    layers: &[PathBuf],
    changes: &[Vec<PathBuf>],
    resets: &[Vec<DirectoryReset>],
) -> Result<PathBuf> {
    let guard = DIRECTORY_OPERATIONS.lock().await;
    let runtime = runtime.to_owned();
    let temporary_mount = temporary_mount.to_owned();
    let layers = layers.to_vec();
    let changes = changes.to_vec();
    let resets = resets.to_vec();
    tokio::spawn(async move {
        let _guard = guard;
        compose_directory_metadata_inner(&runtime, &temporary_mount, &layers, &changes, &resets)
            .await
    })
    .await
    .map_err(|err| eyre!("directory composition task failed: {err}"))?
}

async fn compose_directory_metadata_inner(
    runtime: &Path,
    temporary_mount: &Path,
    layers: &[PathBuf],
    changes: &[Vec<PathBuf>],
    resets: &[Vec<DirectoryReset>],
) -> Result<PathBuf> {
    let metadata_dir = Path::new("/var/lib/odorobo/directory-metadata").join(
        runtime
            .file_name()
            .ok_or_else(|| eyre!("VM runtime has no identity"))?,
    );
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::create_dir_all(&metadata_dir).await?;
    tokio::fs::set_permissions(&metadata_dir, std::fs::Permissions::from_mode(0o700)).await?;
    let tree = metadata_dir.join("tree");
    tokio::fs::create_dir_all(&tree).await?;
    let empty = runtime.join("rootfs-empty-lower");
    tokio::fs::create_dir_all(&empty).await?;
    let mut lowers = layers
        .iter()
        .rev()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>();
    lowers.push(empty.display().to_string());
    run_checked(
        "mount",
        [
            "-t".into(),
            "overlay".into(),
            "overlay".into(),
            "-o".into(),
            format!("ro,nosuid,nodev,lowerdir={}", lowers.join(":")),
        ],
        &[temporary_mount],
    )
    .await?;
    let mut pending = vec![PathBuf::new()];
    let mut directories = Vec::new();
    while let Some(relative) = pending.pop() {
        let path = temporary_mount.join(&relative);
        tokio::fs::create_dir_all(tree.join(&relative)).await?;
        let mut entries = tokio::fs::read_dir(&path).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                pending.push(relative.join(entry.file_name()));
            }
        }
        // The newest explicit directory header wins. If none exists, use
        // the first layer introducing it, never a later synthetic ancestor.
        let selected = directory_metadata_source(&relative, layers, changes, resets)?;
        directories.push((relative, selected));
    }
    directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (relative, selected) in directories {
        copy_directory_attributes(&layers[selected].join(&relative), &tree.join(&relative))?;
    }
    unmount_path(temporary_mount).await?;
    let image = metadata_dir.join("directories.cfs");
    run_checked(
        "mkcomposefs",
        [tree.display().to_string(), image.display().to_string()],
        &[],
    )
    .await?;
    ensure_fsverity(&image).await?;
    let digest = run_checked("composefs-info", ["measure-file".into()], &[&image]).await?;
    let target = runtime.join("rootfs-directory-metadata");
    tokio::fs::create_dir_all(&target).await?;
    run_checked(
        "mount.composefs",
        [
            "-o".into(),
            format!(
                "basedir={},digest={},verity,ro",
                empty.display(),
                digest.trim()
            ),
        ],
        &[&image, &target],
    )
    .await?;
    run_checked(
        "mount",
        ["-o".into(), "remount,bind,ro,nosuid,nodev".into()],
        &[&target],
    )
    .await?;
    run_checked("mount", ["--make-rprivate".into()], &[&target]).await?;
    tokio::fs::remove_dir_all(tree).await?;
    Ok(target)
}

/// Tear down mounts in dependency order. Persistent upper data is never removed
/// here; RBD is unmounted and unmapped only after the overlay and layers stop.
pub async fn unmount_rootfs(prepared: &PreparedRootfs) -> Result<()> {
    let _directory_barrier = DIRECTORY_OPERATIONS.lock().await;
    unmount_scratch_mounts(&prepared.scratch_mounts).await?;
    unmount_path(&prepared.mount).await?;
    unmount_layer_mounts(&prepared.layer_mounts).await?;
    release_persistent_backend(
        prepared.persistent_state_mount.as_deref(),
        prepared.rbd_image.as_deref(),
    )
    .await?;
    if let Some(runtime) = prepared.mount.parent().and_then(Path::file_name) {
        let directory = Path::new("/var/lib/odorobo/directory-metadata").join(runtime);
        match tokio::fs::remove_dir_all(directory).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    if prepared.mode == RootfsMode::Ephemeral {
        for path in [&prepared.upper, &prepared.work].into_iter().flatten() {
            match tokio::fs::remove_dir_all(path).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
    }
    Ok(())
}

async fn run_checked(
    program: &str,
    args: impl IntoIterator<Item = String>,
    extra: &[&Path],
) -> Result<String> {
    let mut cmd = Command::new(program);
    for arg in args {
        cmd.arg(arg);
    }
    for path in extra {
        cmd.arg(path);
    }
    let output = run_owned_command(cmd)
        .await
        .wrap_err_with(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn scratch_size_mb(env: &str, default_mb: u64) -> Result<u64> {
    let value = std::env::var(env).unwrap_or_else(|_| format!("{default_mb}M"));
    parse_scratch_size_mb(env, &value)
}

fn parse_scratch_size_mb(env: &str, value: &str) -> Result<u64> {
    parse_size_bytes(env, value, 2 * 1024_u64.pow(3), false).map(|bytes| bytes / 1024_u64.pow(2))
}

/// Resolve the three scratch locations completely before mounting any tmpfs.
/// This avoids leaving an earlier scratch mount behind when a later path is
/// absent, a symlink, or not a directory in the immutable image.
fn validate_scratch_targets(root: &Path) -> Result<Vec<(PathBuf, String)>> {
    let paths = [
        (
            "tmp",
            format!(
                "size={}m,mode=1777,nosuid,nodev",
                scratch_size_mb("ODOROBO_ROOTFS_TMP_SIZE", 64)?
            ),
        ),
        (
            "run",
            format!(
                "size={}m,mode=755,nosuid,nodev",
                scratch_size_mb("ODOROBO_ROOTFS_RUN_SIZE", 32)?
            ),
        ),
        (
            "var/tmp",
            format!(
                "size={}m,mode=1777,nosuid,nodev",
                scratch_size_mb("ODOROBO_ROOTFS_VARTMP_SIZE", 32)?
            ),
        ),
    ];
    let mut validated = Vec::with_capacity(paths.len());
    for (relative, options) in paths {
        let target = root.join(relative);
        let mut prefix = root.to_path_buf();
        for component in Path::new(relative).components() {
            let std::path::Component::Normal(name) = component else {
                bail!("invalid scratch path {relative}");
            };
            prefix.push(name);
            match std::fs::symlink_metadata(&prefix) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => bail!(
                    "scratch path {} is not a real directory in image",
                    prefix.display()
                ),
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => bail!(
                    "scratch path {} is absent from the immutable image",
                    prefix.display()
                ),
                Err(err) => return Err(err.into()),
            }
        }
        validated.push((target, options));
    }
    Ok(validated)
}

async fn unmount_scratch_mounts(mounts: &[PathBuf]) -> Result<()> {
    unmount_layer_mounts(mounts)
        .await
        .wrap_err("scratch unmount failed")
}

/// Read-only root supports only bounded volatile scratch: /tmp, /run, /var/tmp.
/// Sizes are node policy via ODOROBO_ROOTFS_{TMP,RUN,VARTMP}_SIZE (M/G).
async fn mount_readonly_scratch(root: &Path) -> Result<Vec<PathBuf>> {
    let targets = validate_scratch_targets(root)?;
    // Private propagation prevents nested per-VM tmpfs mounts escaping through
    // shared host mount trees or appearing in another VM's mount view.
    run_checked("mount", ["--make-rprivate".to_owned()], &[root]).await?;
    let mut mounted = Vec::with_capacity(targets.len());
    for (target, options) in targets {
        if let Err(err) = run_checked(
            "mount",
            [
                "-t".into(),
                "tmpfs".into(),
                "-o".into(),
                options,
                "tmpfs".into(),
            ],
            &[target.as_path()],
        )
        .await
        {
            let cleanup = unmount_scratch_mounts(&mounted).await;
            return Err(err.wrap_err(format!(
                "mount scratch tmpfs at {}; rollback: {cleanup:?}",
                target.display()
            )));
        }
        mounted.push(target);
    }
    Ok(mounted)
}

/// Remove persistent upper state only from the explicit DeleteVM path.
pub async fn delete_persistent_rootfs(vmid: &str) -> Result<()> {
    let _operations = PERSISTENT_OPERATIONS.lock().await;
    validate_vmid(vmid)?;
    let _lock = acquire_rootfs_lock(vmid).await?;
    let path = PathBuf::from(PERSISTENT_ROOTFS_ROOT).join(vmid);
    if is_mountpoint(&path).await? {
        bail!(
            "refusing to delete mounted persistent rootfs state {}",
            path.display()
        );
    }
    match persistent_backend()? {
        PersistentBackend::Local => match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(eyre!(
                "remove persistent rootfs state {}: {err}",
                path.display()
            )),
        },
        PersistentBackend::Rbd => {
            let image = rbd_identity(vmid)?;
            cleanup_staging_images(&image).await?;
            if !rbd_image_exists(&image).await? {
                tokio::fs::remove_dir_all(&path).await.ok();
                return Ok(());
            }
            if rbd_lookup_device(&image).await?.is_some() {
                bail!("refusing to delete mapped RBD image {image}; detach it first");
            }
            let remove = rbd_output(&["rm".into(), image.clone()]).await?;
            if !remove.status.success() {
                bail!(
                    "delete RBD rootfs image {image}: {}",
                    String::from_utf8_lossy(&remove.stderr).trim()
                );
            }
            tokio::fs::remove_dir_all(&path).await.ok();
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// virtiofsd supervision (runs inside the VM actor)
// ---------------------------------------------------------------------------

/// Resolve the virtiofsd binary (Fedora ships it in /usr/libexec, not PATH).
pub fn virtiofsd_path() -> Option<PathBuf> {
    if let Ok(output) = std::process::Command::new("which")
        .arg("virtiofsd")
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    let libexec = PathBuf::from("/usr/libexec/virtiofsd");
    libexec.exists().then_some(libexec)
}

/// Everything needed to (re)spawn one virtiofsd instance.
#[derive(Debug, Clone)]
pub struct VirtiofsdSpec {
    /// Binary to run (normally virtiofsd_path(); override in tests).
    pub program: PathBuf,
    /// vhost-user socket the Cloud Hypervisor process connects to.
    pub socket: PathBuf,
    /// Directory (the composefs mount) served to the guest.
    pub shared_dir: PathBuf,
    /// Log file for virtiofsd stdout/stderr.
    pub log: PathBuf,
}

struct SupState {
    shutdown: AtomicBool,
    notify: Notify,
    restarts: AtomicU32,
    ready: AtomicBool,
}

/// Runs virtiofsd as a supervised child of the VM actor for the VM's
/// lifetime: restarts with capped backoff on unexpected exit, stopped with
/// the actor. Children are `kill_on_drop`, so even an abrupt actor kill does
/// not leak the process.
pub struct VirtioFsSupervisor {
    spec: VirtiofsdSpec,
    state: Arc<SupState>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    abort: tokio::task::AbortHandle,
}

impl Drop for VirtioFsSupervisor {
    fn drop(&mut self) {
        // Dropping a JoinHandle alone detaches it. Abort the task so its
        // kill_on_drop child cannot outlive a cancelled startup or actor.
        self.state.shutdown.store(true, Ordering::Relaxed);
        self.abort.abort();
    }
}

impl VirtioFsSupervisor {
    /// Start supervising a virtiofsd instance for this spec.
    pub fn start(spec: VirtiofsdSpec) -> Result<Self> {
        // A stale socket from a previous run would make CH connect to
        // nothing; virtiofsd also refuses to replace an existing socket.
        std::fs::remove_file(&spec.socket).ok();

        let state = Arc::new(SupState {
            shutdown: AtomicBool::new(false),
            notify: Notify::new(),
            restarts: AtomicU32::new(0),
            ready: AtomicBool::new(false),
        });
        let task_state = Arc::clone(&state);
        let task_spec = spec.clone();

        let handle = tokio::spawn(async move {
            let mut backoff_secs: u64 = 1;
            loop {
                if task_state.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                task_state.ready.store(false, Ordering::Relaxed);
                // Only the previous child has owned this private VM socket.
                // Remove it after exit, before each replacement generation.
                std::fs::remove_file(&task_spec.socket).ok();
                let log = match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&task_spec.log)
                {
                    Ok(log) => log,
                    Err(err) => {
                        warn!(
                            error = ?err,
                            log = %task_spec.log.display(),
                            "cannot open virtiofsd log; stopping supervisor"
                        );
                        break;
                    }
                };
                let mut cmd = Command::new(&task_spec.program);
                cmd.args([
                    format!("--socket-path={}", task_spec.socket.display()),
                    format!("--shared-dir={}", task_spec.shared_dir.display()),
                    "--sandbox=chroot".to_owned(),
                    "--cache=auto".to_owned(),
                    "--xattr".to_owned(),
                    "--posix-acl".to_owned(),
                ])
                .stdin(std::process::Stdio::null())
                .stdout(log.try_clone().expect("log handle clone"))
                .stderr(log)
                .kill_on_drop(true);

                debug!(program = %task_spec.program.display(), "spawning virtiofsd");
                match cmd.spawn() {
                    Ok(mut child) => {
                        let exit = loop {
                            match child.try_wait() {
                                Ok(Some(status)) => break Ok(status),
                                Err(err) => break Err(err),
                                Ok(None) => {}
                            }
                            // A pathname from an exited generation is never
                            // readiness. Only the current live child publishes it.
                            task_state
                                .ready
                                .store(task_spec.socket.exists(), Ordering::Relaxed);
                            tokio::select! {
                                () = tokio::time::sleep(Duration::from_millis(50)) => {}
                                _ = task_state.notify.notified() => {
                                    _ = child.start_kill();
                                    break child.wait().await;
                                }
                            }
                        };
                        task_state.ready.store(false, Ordering::Relaxed);
                        std::fs::remove_file(&task_spec.socket).ok();
                        if task_state.shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        warn!(?exit, "virtiofsd exited unexpectedly, restarting");
                    }
                    Err(err) => {
                        warn!(
                            error = ?err,
                            program = %task_spec.program.display(),
                            "failed to spawn virtiofsd"
                        );
                    }
                }
                let restarts = task_state.restarts.fetch_add(1, Ordering::Relaxed);
                warn!(
                    restarts,
                    backoff_secs, "virtiofsd backing off before restart"
                );
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                    () = task_state.notify.notified() => {}
                }
                backoff_secs = backoff_secs.saturating_mul(2).min(30);
            }
            debug!("virtiofsd supervisor loop exiting");
        });

        Ok(Self {
            spec,
            state,
            abort: handle.abort_handle(),
            task: Mutex::new(Some(handle)),
        })
    }

    /// Number of unexpected exits / failed spawns since start
    /// (observability and tests).
    pub fn restarts(&self) -> u32 {
        self.state.restarts.load(Ordering::Relaxed)
    }

    /// Wait until the vhost-user socket exists (virtiofsd creates it at
    /// startup) so CH can attach the fs device.
    pub async fn wait_socket_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.state.ready.load(Ordering::Relaxed) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "virtiofsd socket {} did not appear within {timeout:?} — see {}",
                    self.spec.socket.display(),
                    self.spec.log.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Stop supervising and kill the running child, if any.
    pub async fn stop(&self) {
        self.stop_with_timeout(Duration::from_secs(5)).await;
    }

    async fn stop_with_timeout(&self, timeout: Duration) {
        self.state.shutdown.store(true, Ordering::Relaxed);
        self.state.notify.notify_one();
        // Serialize concurrent stop calls until the child has been reaped.
        let mut slot = self.task.lock().await;
        if let Some(task) = slot.as_mut() {
            // Give the loop a moment to observe the shutdown flag and reap
            // the child; abort (kill_on_drop) as a backstop.
            if tokio::time::timeout(timeout, &mut *task).await.is_err() {
                warn!("virtiofsd supervisor task did not exit in time; aborting");
                task.abort();
                _ = task.await;
            }
            *slot = None;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        Compression, OciLayerDescriptor, VirtioFsSupervisor, VirtiofsdSpec, apply_whiteout,
        compression_of_media_type, digest_blob_path, ensure_no_symlink_ancestors, hash_file,
        materialize_layer_at, read_oci_manifest, safe_rel_path, sanitize_image_ref, to_hex,
    };
    use sha2::Digest;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    #[test]
    fn unpack_preserves_oci_metadata_and_rejects_overlay_xattrs() {
        let base =
            std::env::temp_dir().join(format!("odorobo-metadata-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base).unwrap();
        let mut bytes = Vec::new();
        let mut archive = tar::Builder::new(&mut bytes);
        // A valid v2 Linux capability xattr (CAP_NET_BIND_SERVICE).
        let capability = [1, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        archive
            .append_pax_extensions([("SCHILY.xattr.security.capability", capability.as_slice())])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_uid(1234);
        header.set_gid(2345);
        header.set_mode(0o4755);
        header.set_size(4);
        header.set_cksum();
        archive
            .append_data(&mut header, "app", &b"data"[..])
            .unwrap();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o1777);
        header.set_cksum();
        archive
            .append_data(&mut header, "tmp", std::io::empty())
            .unwrap();
        archive.finish().unwrap();
        drop(archive);
        super::unpack_layer(bytes.as_slice(), Compression::None, &base, 1024 * 1024, 10).unwrap();
        let meta = std::fs::metadata(base.join("app")).unwrap();
        assert_eq!(
            (meta.uid(), meta.gid(), meta.mode() & 0o7777),
            (1234, 2345, 0o4755)
        );
        assert_eq!(
            std::fs::metadata(base.join("tmp")).unwrap().mode() & 0o7777,
            0o1777
        );
        let path = std::ffi::CString::new(base.join("app").as_os_str().as_encoded_bytes()).unwrap();
        let name = c"security.capability";
        let mut actual = [0; 20];
        // SAFETY: valid path/name and output buffer.
        let len = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                actual.as_mut_ptr().cast(),
                actual.len(),
            )
        };
        assert_eq!(len, 20);
        assert_eq!(actual, capability);

        let mut bytes = Vec::new();
        let mut archive = tar::Builder::new(&mut bytes);
        archive
            .append_pax_extensions([("SCHILY.xattr.trusted.overlay.redirect", b"/etc".as_slice())])
            .unwrap();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        archive
            .append_data(&mut header, "hostile", std::io::empty())
            .unwrap();
        archive.finish().unwrap();
        drop(archive);
        assert!(
            super::unpack_layer(bytes.as_slice(), Compression::None, &base, 1024 * 1024, 10)
                .is_err()
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn root_metadata_and_default_acls_are_order_independent() {
        // Linux ACL xattr v2: owner rwx, named uid 1234 rwx, group r-x,
        // mask rwx, other r-x. Children without an ACL must not inherit it.
        let mut acl = 2_u32.to_le_bytes().to_vec();
        for (tag, permission, id) in [
            (1_u16, 7_u16, u32::MAX),
            (2, 7, 1234),
            (4, 5, u32::MAX),
            (16, 7, u32::MAX),
            (32, 5, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(permission.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        for reversed in [false, true] {
            let base =
                std::env::temp_dir().join(format!("odorobo-root-acl-{}", ulid::Ulid::generate()));
            std::fs::create_dir_all(&base).unwrap();
            let mut bytes = Vec::new();
            let mut archive = tar::Builder::new(&mut bytes);
            let mut header = tar::Header::new_gnu();
            header.set_uid(1234);
            header.set_gid(2345);
            header.set_mode(0o755);
            header.set_size(0);
            if reversed {
                header.set_cksum();
                archive
                    .append_data(&mut header, "child", std::io::empty())
                    .unwrap();
            }
            archive
                .append_pax_extensions([("SCHILY.xattr.system.posix_acl_default", acl.as_slice())])
                .unwrap();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o1775);
            header.set_mtime(42);
            header.set_cksum();
            archive
                .append_data(&mut header, "./", std::io::empty())
                .unwrap();
            if !reversed {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0o755);
                header.set_cksum();
                archive
                    .append_data(&mut header, "child", std::io::empty())
                    .unwrap();
            }
            archive.finish().unwrap();
            drop(archive);
            super::unpack_layer(bytes.as_slice(), Compression::None, &base, 1024 * 1024, 10)
                .unwrap();
            let meta = std::fs::metadata(&base).unwrap();
            assert_eq!(
                (meta.uid(), meta.gid(), meta.mode() & 0o7777, meta.mtime()),
                (1234, 2345, 0o1775, 42)
            );
            let child =
                std::ffi::CString::new(base.join("child").as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(
                // SAFETY: valid C strings; null buffer queries xattr existence.
                unsafe {
                    libc::lgetxattr(
                        child.as_ptr(),
                        c"system.posix_acl_access".as_ptr(),
                        std::ptr::null_mut(),
                        0,
                    )
                },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ENODATA)
            );
            std::fs::remove_dir_all(base).unwrap();
        }
    }

    #[tokio::test]
    async fn cancelled_rbd_caller_cannot_overtake_its_command() {
        let base =
            std::env::temp_dir().join(format!("odorobo-rbd-cancel-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base).unwrap();
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("echo $$ > \"$1/pid\"; sleep 0.2; echo mapped > \"$1/mapped\"")
            .arg("sh")
            .arg(&base);
        let caller = tokio::spawn(super::run_owned_command(command));
        for _ in 0..100 {
            if base.join("pid").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(base.join("pid").exists());
        caller.abort();
        _ = caller.await;
        // Reconciliation must wait for the detached owned command, then sees
        // its effects; no mapping can appear after cleanup has inspected them.
        let guard = super::TOOL_OPERATIONS.lock().await;
        assert!(base.join("mapped").exists());
        let pid: u32 = std::fs::read_to_string(base.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        std::fs::remove_file(base.join("mapped")).unwrap();
        drop(guard);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!base.join("mapped").exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn cancelled_mount_helper_descendant_cannot_overtake_teardown() {
        let base =
            std::env::temp_dir().join(format!("odorobo-mount-cancel-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base).unwrap();
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("(echo ready > \"$1/ready\"; sleep 0.2; echo acquired > \"$1/acquired\") & exit 0")
            .arg("sh")
            .arg(&base);
        let caller = tokio::spawn(super::run_owned_command(command));
        for _ in 0..100 {
            if base.join("ready").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(base.join("ready").exists());
        caller.abort();
        _ = caller.await;
        super::unmount_path(&base).await.unwrap();
        assert!(
            base.join("acquired").exists(),
            "teardown must wait for helper descendants"
        );
        std::fs::remove_file(base.join("acquired")).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!base.join("acquired").exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn supervisor_replaces_stale_socket_generation() {
        let base =
            std::env::temp_dir().join(format!("odorobo-socket-restart-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base).unwrap();
        let script = base.join("stub");
        std::fs::write(&script, "#!/usr/bin/python3\nimport socket,sys,time,pathlib\np=pathlib.Path(__file__).parent\ns=socket.socket(socket.AF_UNIX)\ns.bind(str(p/'sock'))\nf=p/'generation'\nn=int(f.read_text())+1 if f.exists() else 1\nf.write_text(str(n))\ntime.sleep(0.15 if n==1 else 100)\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let supervisor = VirtioFsSupervisor::start(VirtiofsdSpec {
            program: script,
            socket: base.join("sock"),
            shared_dir: base.clone(),
            log: base.join("log"),
        })
        .unwrap();
        supervisor
            .wait_socket_ready(Duration::from_secs(2))
            .await
            .unwrap();
        for _ in 0..100 {
            if supervisor.restarts() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(supervisor.restarts() > 0);
        assert!(
            !supervisor
                .state
                .ready
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        supervisor
            .wait_socket_ready(Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(base.join("generation")).unwrap(),
            "2"
        );
        supervisor.stop().await;
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn teardown_accepts_already_unmounted_paths() {
        let base =
            std::env::temp_dir().join(format!("odorobo-teardown-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base).unwrap();
        super::unmount_layer_mounts(&[base.clone(), base.join("missing")])
            .await
            .unwrap();
        super::release_persistent_backend(Some(&base), None)
            .await
            .unwrap();
        super::release_persistent_backend(Some(&base), None)
            .await
            .unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires privileged tmpfs mounts; tests busy-layer rollback and purge protection"]
    async fn partial_teardown_retries_without_purging_live_mounts() {
        let vmid = ulid::Ulid::generate().to_string();
        let mut instance =
            crate::ch_driver::VMInstance::new(&vmid, PathBuf::from("/unused"), None, None);
        let runtime = instance.runtime_dir();
        let mount = runtime.join("rootfs");
        let layer = runtime.join("rootfs-layers/0");
        let upper = runtime.join("rootfs.upper");
        for path in [&mount, &layer, &upper] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(upper.join("keep"), b"persistent until detached").unwrap();
        for path in [&mount, &layer] {
            super::run_checked(
                "mount",
                ["-t".into(), "tmpfs".into(), "tmpfs".into()],
                &[path],
            )
            .await
            .unwrap();
        }
        std::fs::write(layer.join("keep"), b"never purge through a mount").unwrap();
        let mut busy = tokio::process::Command::new("sleep")
            .arg("100")
            .current_dir(&layer)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let prepared = super::PreparedRootfs {
            digest: "fixture".into(),
            mount: mount.clone(),
            upper: Some(upper.clone()),
            work: None,
            layer_mounts: vec![layer.clone()],
            persistent_state_mount: None,
            rbd_image: None,
            _state_lock: super::acquire_rootfs_lock(&vmid).await.unwrap(),
            mode: crate::manifest::RootfsMode::Ephemeral,
            scratch_mounts: vec![],
        };
        let result = async {
            assert!(super::unmount_rootfs(&prepared).await.is_err());
            assert!(!super::is_mountpoint(&mount).await.unwrap());
            assert!(super::is_mountpoint(&layer).await.unwrap());
            assert!(instance.purge_instance_data().is_err());
            assert!(layer.join("keep").exists());
            assert!(upper.join("keep").exists());
            busy.kill().await.unwrap();
            super::unmount_rootfs(&prepared).await.unwrap();
            super::unmount_rootfs(&prepared).await.unwrap();
            assert!(!upper.exists());
            instance.purge_instance_data().unwrap();
        }
        .await;
        _ = busy.kill().await;
        _ = super::unmount_rootfs(&prepared).await;
        _ = instance.purge_instance_data();
        result
    }

    #[tokio::test]
    async fn supervisor_timeout_aborts_and_joins_task() {
        let state = std::sync::Arc::new(super::SupState {
            shutdown: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
            restarts: std::sync::atomic::AtomicU32::new(0),
            ready: std::sync::atomic::AtomicBool::new(false),
        });
        let task = tokio::spawn(std::future::pending::<()>());
        let abort = task.abort_handle();
        let supervisor = VirtioFsSupervisor {
            spec: VirtiofsdSpec {
                program: "/bin/false".into(),
                socket: "unused".into(),
                shared_dir: "unused".into(),
                log: "unused".into(),
            },
            state,
            task: tokio::sync::Mutex::new(Some(task)),
            abort: abort.clone(),
        };
        supervisor.stop_with_timeout(Duration::from_millis(1)).await;
        assert!(
            abort.is_finished(),
            "timeout must not detach the supervisor"
        );
        supervisor.stop().await;
    }

    #[test]
    fn sanitizes_image_refs_for_the_cache() {
        assert_eq!(
            sanitize_image_ref("docker://registry.fedoraproject.org/fedora:41"),
            "docker___registry.fedoraproject.org_fedora_41"
        );
        assert_eq!(sanitize_image_ref("busybox:latest"), "busybox_latest");
        // The slug is always a single path component: no separators, so it
        // cannot traverse out of the cache root.
        let slug = sanitize_image_ref("../../etc/shadow");
        assert!(!slug.contains('/') && !slug.contains(':'));
        assert_ne!(slug, "..");
        // Pure traversal names must not survive as-is.
        assert_eq!(sanitize_image_ref(".."), "_");
        assert_eq!(sanitize_image_ref("."), "_");
        assert_eq!(sanitize_image_ref(""), "_");
    }

    #[test]
    fn docker_digest_reference_replaces_tag_without_losing_registry_port() {
        assert_eq!(
            super::docker_digest_ref(
                "docker://registry.example:5443/org/image:stable",
                "sha256:abc"
            )
            .unwrap(),
            "docker://registry.example:5443/org/image@sha256:abc"
        );
        assert_eq!(
            super::docker_digest_ref("docker://registry.example/org/image", "sha256:def").unwrap(),
            "docker://registry.example/org/image@sha256:def"
        );
    }

    #[test]
    fn image_ref_cache_uses_full_ref_digest_to_avoid_slug_collisions() {
        let a = "docker://registry.example/a:b";
        let b = "docker://registry.example/a/b";
        assert_eq!(sanitize_image_ref(a), sanitize_image_ref(b));
        assert_ne!(
            to_hex(&sha2::Sha256::digest(a.as_bytes())),
            to_hex(&sha2::Sha256::digest(b.as_bytes()))
        );
    }

    #[test]
    fn global_blob_cas_hardlinks_equal_descriptor_content_across_layouts() {
        let base =
            std::env::temp_dir().join(format!("odorobo-blob-cas-{}", ulid::Ulid::generate()));
        let cas = base.join("cas");
        let payload = format!("blob-{}", ulid::Ulid::generate()).into_bytes();
        let mut layouts = Vec::new();
        for name in ["ref-a", "ref-b"] {
            let layout = base.join(name);
            let path = layout.join("blobs/sha256");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join(to_hex(&sha2::Sha256::digest(&payload))), &payload).unwrap();
            super::globalize_layout_blobs_at(&layout, &cas).unwrap();
            layouts.push(layout);
        }
        let digest = to_hex(&sha2::Sha256::digest(&payload));
        let canonical = std::fs::metadata(cas.join(&digest)).unwrap();
        for layout in layouts {
            let copy = std::fs::metadata(layout.join("blobs/sha256").join(&digest)).unwrap();
            assert_eq!(
                copy.ino(),
                canonical.ino(),
                "reference layouts must share the CAS inode"
            );
            assert!(copy.permissions().readonly());
        }
        // Corrupt reference blob data is rejected rather than added to cache.
        let bad = base.join("bad");
        let bad_blob_dir = bad.join("blobs/sha256");
        std::fs::create_dir_all(&bad_blob_dir).unwrap();
        std::fs::write(bad_blob_dir.join(&digest), b"tampered").unwrap();
        assert!(super::globalize_layout_blobs_at(&bad, &cas).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn validates_digest_hex_before_path_use() {
        let layout = Path::new("/cache");
        let good = "a".repeat(64);
        assert_eq!(
            digest_blob_path(layout, &format!("sha256:{good}"))
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy(),
            good
        );
        assert!(digest_blob_path(layout, "sha256:../../etc/shadow").is_err());
        assert!(digest_blob_path(layout, "sha256:deadbeef").is_err());
        assert!(digest_blob_path(layout, "md5:aaaa").is_err());
        assert!(digest_blob_path(layout, "garbage").is_err());
    }

    #[test]
    fn unpacking_through_a_planted_symlink_is_refused() {
        let base = std::env::temp_dir().join(format!("odorobo-symlink-{}", ulid::Ulid::generate()));
        let tree = base.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        // Layer 1 plants a symlink; layer 2 must not resolve through it.
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", tree.join("etc")).unwrap();

        assert!(ensure_no_symlink_ancestors(&tree, Path::new("etc/passwd")).is_err());
        assert!(ensure_no_symlink_ancestors(&tree, Path::new("etc")).is_err());
        assert!(ensure_no_symlink_ancestors(&tree, Path::new("usr/bin")).is_ok());
        // Whiteouts through the planted symlink are refused too.
        assert!(apply_whiteout(&tree, Path::new("etc"), ".wh..wh..opk").is_err());

        std::fs::remove_dir_all(&base).unwrap();
    }

    fn put_blob(layout: &Path, bytes: &[u8]) -> String {
        use sha2::Digest;
        let digest = format!("sha256:{}", to_hex(&sha2::Sha256::digest(bytes)));
        let path = digest_blob_path(layout, &digest).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        digest
    }

    fn fixture_oci_layout(base: &Path) -> (PathBuf, OciLayerDescriptor) {
        fixture_oci_layout_with_fill(base, b'x')
    }

    fn fixture_oci_layout_with_fill(base: &Path, fill: u8) -> (PathBuf, OciLayerDescriptor) {
        fixture_oci_layout_with_options(base, fill, true)
    }

    fn fixture_oci_layout_with_options(
        base: &Path,
        fill: u8,
        include_run: bool,
    ) -> (PathBuf, OciLayerDescriptor) {
        fixture_oci_layout_with_options_and_init(base, fill, include_run, None)
    }

    pub(crate) fn fixture_oci_layout_with_init(base: &Path, init_binary: &Path) -> PathBuf {
        fixture_oci_layout_with_options_and_init(base, b'x', true, Some(init_binary)).0
    }

    fn fixture_oci_layout_with_options_and_init(
        base: &Path,
        fill: u8,
        include_run: bool,
        init_binary: Option<&Path>,
    ) -> (PathBuf, OciLayerDescriptor) {
        use serde_json::json;
        use sha2::Digest;
        let layout = base.join("layout");
        std::fs::create_dir_all(&layout).unwrap();
        let mut tar_bytes = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut tar_bytes);
            for (path, mode) in [
                ("etc/", 0o755),
                ("bin/", 0o755),
                ("tmp/", 0o1777),
                ("run/", 0o755),
                ("var/", 0o755),
                ("var/tmp/", 0o1777),
            ]
            .into_iter()
            .filter(|(path, _)| {
                (include_run || *path != "run/") && (init_binary.is_some() || *path != "bin/")
            }) {
                let mut directory = tar::Header::new_gnu();
                directory.set_uid(0);
                directory.set_gid(0);
                directory.set_entry_type(tar::EntryType::Directory);
                directory.set_size(0);
                directory.set_mode(mode);
                directory.set_cksum();
                tar.append_data(&mut directory, path, std::io::empty())
                    .unwrap();
            }
            let payload = vec![fill; 16 * 1024];
            let mut header = tar::Header::new_gnu();
            header.set_uid(0);
            header.set_gid(0);
            header.set_size(payload.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, "etc/x", payload.as_slice())
                .unwrap();
            if let Some(init_binary) = init_binary {
                let binary = std::fs::read(init_binary).unwrap();
                let mut init = tar::Header::new_gnu();
                init.set_uid(0);
                init.set_gid(0);
                init.set_size(binary.len() as u64);
                init.set_mode(0o755);
                init.set_cksum();
                tar.append_data(&mut init, "bin/sh", binary.as_slice())
                    .unwrap();
            }
            let mut whiteout = tar::Header::new_gnu();
            whiteout.set_uid(0);
            whiteout.set_gid(0);
            whiteout.set_size(0);
            whiteout.set_mode(0o000);
            whiteout.set_cksum();
            tar.append_data(&mut whiteout, "etc/.wh.deleted", std::io::empty())
                .unwrap();
            tar.finish().unwrap();
        }
        let layer_digest = put_blob(&layout, &tar_bytes);
        let diff_id = format!("sha256:{}", to_hex(&sha2::Sha256::digest(&tar_bytes)));
        let config = serde_json::to_vec(&json!({"architecture": super::normalized_arch(std::env::consts::ARCH), "os":"linux", "rootfs":{"type":"layers","diff_ids":[diff_id.clone()]}})).unwrap();
        let config_digest = put_blob(&layout, &config);
        let manifest = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config_digest,"size":config.len()},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":layer_digest,"size":tar_bytes.len()}]})).unwrap();
        let manifest_digest = put_blob(&layout, &manifest);
        let platform_arch = super::normalized_arch(std::env::consts::ARCH);
        let other_arch = if platform_arch == "amd64" {
            "arm64"
        } else {
            "amd64"
        };
        let index = serde_json::to_vec(&json!({"schemaVersion":2,"manifests":[
            {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":manifest_digest,"size":manifest.len(),"platform":{"os":"linux","architecture":other_arch}},
            {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":manifest_digest,"size":manifest.len(),"platform":{"os":"linux","architecture":platform_arch},"annotations":{"org.opencontainers.image.ref.name":"latest"}}
        ]})).unwrap();
        std::fs::write(layout.join("index.json"), index).unwrap();
        std::fs::write(
            layout.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        (
            layout,
            OciLayerDescriptor {
                digest: layer_digest,
                diff_id,
                media_type: "application/vnd.oci.image.layer.v1.tar".into(),
                size: Some(tar_bytes.len() as u64),
            },
        )
    }

    #[tokio::test]
    async fn compressed_pull_limit_rejects_and_removes_partial_layout() {
        if std::process::Command::new("skopeo")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let base =
            std::env::temp_dir().join(format!("odorobo-pull-limit-{}", ulid::Ulid::generate()));
        let (layout, _) = fixture_oci_layout(&base);
        let image_ref = format!("oci:{}:latest", layout.display());
        std::fs::create_dir_all(super::OCI_CACHE_ROOT).unwrap();
        let alias = PathBuf::from(super::OCI_CACHE_ROOT).join(format!(
            "{}-{}",
            sanitize_image_ref(&image_ref),
            to_hex(&sha2::Sha256::digest(image_ref.as_bytes()))
        ));
        assert!(
            super::pull_oci(&image_ref, 1).await.is_err(),
            "large image must fail the compressed-byte cap"
        );
        assert!(
            !alias.join("index.json").exists(),
            "an over-limit layout must not be published"
        );
        let leftovers = std::fs::read_dir(PathBuf::from(super::OCI_CACHE_ROOT))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".pull-"))
            .count();
        assert_eq!(leftovers, 0, "partial pull directories must be removed");
        if alias.exists() {
            std::fs::remove_dir_all(alias).unwrap();
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn cancelled_builder_does_not_remove_a_live_staging_writer() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let path = std::env::temp_dir().join(format!(
            "odorobo-extraction-cancel-{}",
            ulid::Ulid::generate()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let cleanup = super::RemovePathOnDrop::new(path.clone());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let writer_path = path.clone();
        let builder = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let cleanup = cleanup;
                started_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                std::fs::create_dir_all(writer_path.join("tree/children")).unwrap();
                std::fs::write(
                    writer_path.join("tree/children/payload"),
                    b"privileged payload",
                )
                .unwrap();
                ((), cleanup)
            })
            .await
            .unwrap()
        });
        started_rx.await.unwrap();
        builder.abort();
        _ = builder.await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        resume_tx.send(()).unwrap();
        for _ in 0..100 {
            if !path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!path.exists(), "last writer must remove abandoned staging");
    }

    #[test]
    fn temporary_pull_guard_removes_cancelled_artifact_path() {
        let path =
            std::env::temp_dir().join(format!("odorobo-cancel-cleanup-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("partial"), b"partial").unwrap();
        drop(super::RemovePathOnDrop::new(path.clone()));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn verifies_manifest_platform_config_and_layer_descriptors() {
        let base =
            std::env::temp_dir().join(format!("odorobo-oci-index-{}", ulid::Ulid::generate()));
        let (layout, layer) = fixture_oci_layout(&base);
        let (_, descriptors) = read_oci_manifest(&layout).unwrap();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].digest, layer.digest);
        let mut bad_diff_id = layer.clone();
        bad_diff_id.diff_id = format!("sha256:{}", "b".repeat(64));
        assert!(
            materialize_layer_at(&layout, &bad_diff_id, &base.join("cache-diffid"))
                .await
                .is_err()
        );
        assert_eq!(
            hash_file(&digest_blob_path(&layout, &layer.digest).unwrap()).unwrap(),
            layer.digest
        );
        // Any manifest, config, or layer content change invalidates its descriptor.
        let blob = digest_blob_path(&layout, &layer.digest).unwrap();
        let mut bytes = std::fs::read(&blob).unwrap();
        bytes[0] ^= 1;
        std::fs::write(&blob, bytes).unwrap();
        assert!(
            materialize_layer_at(&layout, &layer, &base.join("cache"))
                .await
                .is_err()
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn materializes_and_reuses_one_immutable_fsverity_layer_cache_entry() {
        if std::process::Command::new("mkcomposefs")
            .arg("--help")
            .output()
            .is_err()
            || std::process::Command::new("fsverity")
                .arg("--help")
                .output()
                .is_err()
        {
            return;
        }
        let base = std::path::PathBuf::from("/var/lib/odorobo/test-layers")
            .join(ulid::Ulid::generate().to_string());
        let (layout, layer) = fixture_oci_layout(&base);
        let second_layout = base.join("other-reference");
        std::fs::create_dir_all(&second_layout).unwrap();
        let blob = digest_blob_path(&layout, &layer.digest).unwrap();
        let second_blob = digest_blob_path(&second_layout, &layer.digest).unwrap();
        std::fs::create_dir_all(second_blob.parent().unwrap()).unwrap();
        std::fs::copy(&blob, &second_blob).unwrap();
        let cache = base.join("cache");
        let first = materialize_layer_at(&layout, &layer, &cache).await.unwrap();
        let second = materialize_layer_at(&second_layout, &layer, &cache)
            .await
            .unwrap();
        assert_eq!(
            first, second,
            "same OCI layer digest must reuse its composefs entry"
        );
        assert_eq!(
            std::fs::read_to_string(first.join("oci-digest"))
                .unwrap()
                .trim(),
            layer.digest
        );
        assert!(
            !first.join("tree").exists(),
            "temporary extraction must not be retained"
        );
        assert!(
            std::process::Command::new("fsverity")
                .arg("digest")
                .arg(first.join("layer.cfs"))
                .status()
                .unwrap()
                .success()
        );
        let stored_files = std::fs::read_dir(first.join("store"))
            .unwrap()
            .flat_map(|prefix| std::fs::read_dir(prefix.unwrap().path()).unwrap())
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert!(
            !stored_files.is_empty(),
            "large file content must live in the object store"
        );
        for object in stored_files {
            assert!(
                std::process::Command::new("fsverity")
                    .arg("digest")
                    .arg(&object)
                    .status()
                    .unwrap()
                    .success(),
                "backing payload must have fs-verity enabled"
            );
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn read_only_scratch_size_policy_is_bounded_and_configurable() {
        assert_eq!(super::parse_scratch_size_mb("tmp", "64M").unwrap(), 64);
        assert_eq!(super::parse_scratch_size_mb("run", "2G").unwrap(), 2048);
        assert!(super::parse_scratch_size_mb("tmp", "0M").is_err());
        assert!(super::parse_scratch_size_mb("tmp", "3G").is_err());
        assert!(super::parse_scratch_size_mb("tmp", "1K").is_err());
    }

    #[test]
    fn scratch_paths_are_prevalidated_before_any_mounting() {
        let base = std::env::temp_dir().join(format!(
            "odorobo-scratch-preflight-{}",
            ulid::Ulid::generate()
        ));
        std::fs::create_dir_all(base.join("tmp")).unwrap();
        let before = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        assert!(
            super::validate_scratch_targets(&base).is_err(),
            "missing /run and /var/tmp must fail preflight"
        );
        assert_eq!(
            before,
            std::fs::read_to_string("/proc/self/mountinfo").unwrap(),
            "preflight must not mount the valid earlier /tmp path"
        );
        std::fs::create_dir_all(base.join("run")).unwrap();
        std::fs::create_dir_all(base.join("var/tmp")).unwrap();
        std::fs::remove_dir(base.join("run")).unwrap();
        std::os::unix::fs::symlink("/tmp", base.join("run")).unwrap();
        assert!(
            super::validate_scratch_targets(&base).is_err(),
            "symlink scratch targets must reject"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn rbd_size_policy_is_bounded_and_validated() {
        assert_eq!(super::parse_rbd_size("64M").unwrap(), "64M");
        assert_eq!(super::parse_rbd_size("1G").unwrap(), "1G");
        assert!(super::parse_rbd_size("63M").is_err());
        assert!(super::parse_rbd_size("65G").is_err());
        assert!(super::parse_rbd_size("1Gjunk").is_err());
        assert!(super::parse_rbd_size("-1G").is_err());
    }

    #[test]
    fn maps_layer_media_types_to_compression() {
        assert_eq!(
            compression_of_media_type("application/vnd.oci.image.layer.v1.tar+gzip"),
            Some(Compression::Gzip)
        );
        assert_eq!(
            compression_of_media_type("application/vnd.oci.image.layer.v1.tar+zstd"),
            Some(Compression::Zstd)
        );
        assert_eq!(
            compression_of_media_type("application/vnd.oci.image.layer.v1.tar"),
            Some(Compression::None)
        );
        assert_eq!(
            compression_of_media_type("application/vnd.docker.image.rootfs.diff.tar.gzip"),
            Some(Compression::Gzip)
        );
        assert_eq!(
            compression_of_media_type("application/vnd.oci.image.index"),
            None
        );
    }

    #[test]
    fn rejects_unsupported_special_entries_outside_dev_but_ignores_dev_nodes() {
        let base =
            std::env::temp_dir().join(format!("odorobo-special-tar-{}", ulid::Ulid::generate()));
        let tree = base.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let mut fifo_tar = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut fifo_tar);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Fifo);
            header.set_size(0);
            header.set_mode(0o600);
            header.set_cksum();
            tar.append_data(&mut header, "run/host-socket", std::io::empty())
                .unwrap();
            tar.finish().unwrap();
        }
        assert!(
            super::unpack_layer(
                fifo_tar.as_slice(),
                Compression::None,
                &tree,
                1024 * 1024,
                10
            )
            .is_err()
        );

        let mut device_tar = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut device_tar);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Char);
            header.set_size(0);
            header.set_mode(0o666);
            header.set_device_major(1).unwrap();
            header.set_device_minor(3).unwrap();
            header.set_cksum();
            tar.append_data(&mut header, "dev/null", std::io::empty())
                .unwrap();
            tar.finish().unwrap();
        }
        assert!(
            super::unpack_layer(
                device_tar.as_slice(),
                Compression::None,
                &tree,
                1024 * 1024,
                10
            )
            .is_ok()
        );
        assert!(
            !tree.join("dev/null").exists(),
            "guest devtmpfs supplies /dev nodes"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn rejects_non_dev_special_tar_entries_and_skips_dev_nodes() {
        let base =
            std::env::temp_dir().join(format!("odorobo-special-tar-{}", ulid::Ulid::generate()));
        let tree = base.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let mut fifo_tar = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut fifo_tar);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Fifo);
            header.set_size(0);
            header.set_mode(0o600);
            header.set_cksum();
            tar.append_data(&mut header, "run/host-socket", std::io::empty())
                .unwrap();
            tar.finish().unwrap();
        }
        assert!(
            super::unpack_layer(
                fifo_tar.as_slice(),
                Compression::None,
                &tree,
                1024 * 1024,
                10
            )
            .is_err()
        );

        let mut device_tar = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut device_tar);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Char);
            header.set_size(0);
            header.set_mode(0o666);
            header.set_device_major(1).unwrap();
            header.set_device_minor(3).unwrap();
            header.set_cksum();
            tar.append_data(&mut header, "dev/null", std::io::empty())
                .unwrap();
            tar.finish().unwrap();
        }
        assert!(
            super::unpack_layer(
                device_tar.as_slice(),
                Compression::None,
                &tree,
                1024 * 1024,
                10
            )
            .is_ok()
        );
        assert!(
            !tree.join("dev/null").exists(),
            "guest devtmpfs supplies /dev nodes"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn rejects_tar_path_traversal() {
        assert!(safe_rel_path(Path::new("etc/motd")).is_some());
        assert!(safe_rel_path(Path::new("/etc/motd")).is_some());
        assert!(safe_rel_path(Path::new("./etc/motd")).is_some());
        assert!(safe_rel_path(Path::new("./")).is_some());
        assert!(safe_rel_path(Path::new("../escape")).is_none());
        assert!(safe_rel_path(Path::new("a/../../escape")).is_none());
    }

    #[test]
    fn tar_layer_extraction_preserves_whiteouts_and_directory_metadata() {
        use std::os::unix::fs::MetadataExt;
        let base =
            std::env::temp_dir().join(format!("odorobo-layer-tar-{}", ulid::Ulid::generate()));
        let tree = base.join("layer");
        std::fs::create_dir_all(&tree).unwrap();
        let mut bytes = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut bytes);
            for (path, mode) in [("restricted/", 0o700), ("restricted/opaque/", 0o755)] {
                let mut dir = tar::Header::new_gnu();
                dir.set_uid(0);
                dir.set_gid(0);
                dir.set_entry_type(tar::EntryType::Directory);
                dir.set_size(0);
                dir.set_mode(mode);
                dir.set_cksum();
                tar.append_data(&mut dir, path, std::io::empty()).unwrap();
            }
            for marker in [
                "restricted/.wh.removed",
                "restricted/.wh.gone",
                "restricted/opaque/.wh..wh..opq",
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_mode(0o000);
                header.set_cksum();
                tar.append_data(&mut header, marker, std::io::empty())
                    .unwrap();
            }
            let mut file = tar::Header::new_gnu();
            file.set_uid(0);
            file.set_gid(0);
            file.set_size(4);
            file.set_mode(0o644);
            file.set_cksum();
            tar.append_data(&mut file, "restricted/removed", &b"same"[..])
                .unwrap();
            tar.finish().unwrap();
        }
        let stats =
            super::unpack_layer(bytes.as_slice(), Compression::None, &tree, 1024 * 1024, 10)
                .unwrap();
        assert_eq!(stats.entry_count, 6);
        assert!(stats.uncompressed_bytes <= 1024 * 1024);
        let oversized_tree = base.join("oversized");
        std::fs::create_dir_all(&oversized_tree).unwrap();
        assert!(
            super::unpack_layer(
                bytes.as_slice(),
                Compression::None,
                &oversized_tree,
                1024,
                10
            )
            .is_err()
        );
        let too_many_tree = base.join("too-many");
        std::fs::create_dir_all(&too_many_tree).unwrap();
        assert!(
            super::unpack_layer(
                bytes.as_slice(),
                Compression::None,
                &too_many_tree,
                1024 * 1024,
                2
            )
            .is_err()
        );
        assert_eq!(
            std::fs::metadata(tree.join("restricted")).unwrap().mode() & 0o777,
            0o700
        );
        // Same-layer content wins over a whiteout, independent of tar order.
        assert_eq!(
            std::fs::read(tree.join("restricted/removed")).unwrap(),
            b"same"
        );
        let whiteout = tree.join("restricted/gone");
        assert!(std::fs::metadata(&whiteout).unwrap().is_file());
        assert_eq!(std::fs::metadata(&whiteout).unwrap().len(), 0);
        let whiteout_path =
            std::ffi::CString::new(whiteout.as_os_str().as_encoded_bytes()).unwrap();
        let whiteout_attr = std::ffi::CString::new("trusted.overlay.whiteout").unwrap();
        // SAFETY: C strings and a null buffer with zero length are valid for getxattr.
        let whiteout_attr_len = unsafe {
            libc::getxattr(
                whiteout_path.as_ptr(),
                whiteout_attr.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        assert!(whiteout_attr_len >= 0);
        let opaque_path = std::ffi::CString::new(
            tree.join("restricted/opaque")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();
        let opaque_attr = std::ffi::CString::new("trusted.overlay.opaque").unwrap();
        let mut value = [0_u8; 8];
        // SAFETY: C strings and output buffer are valid for this getxattr call.
        let len = unsafe {
            libc::getxattr(
                opaque_path.as_ptr(),
                opaque_attr.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert!(len >= 0);
        assert_eq!(&value[..usize::try_from(len).unwrap()], b"y");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn whiteouts_are_preserved_as_overlay_markers_in_layer_tree() {
        let base =
            std::env::temp_dir().join(format!("odorobo-whiteout-{}", ulid::Ulid::generate()));
        let tree = base.join("tree");
        std::fs::create_dir_all(tree.join("bin")).unwrap();
        std::fs::write(tree.join("bin/cat"), b"preserved upper entry").unwrap();
        std::fs::write(tree.join("keep"), b"keep").unwrap();

        apply_whiteout(&tree, Path::new("bin"), ".wh.ls").unwrap();
        let whiteout = tree.join("bin/ls");
        assert!(std::fs::metadata(&whiteout).unwrap().is_file());
        assert_eq!(std::fs::metadata(&whiteout).unwrap().len(), 0);
        let path = std::ffi::CString::new(whiteout.as_os_str().as_encoded_bytes()).unwrap();
        let name = std::ffi::CString::new("trusted.overlay.whiteout").unwrap();
        // SAFETY: C strings and null xattr buffer/zero length are valid.
        let whiteout_attr_size =
            unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        assert!(whiteout_attr_size >= 0);
        assert!(tree.join("bin/cat").exists());

        apply_whiteout(&tree, Path::new("bin"), ".wh..wh..opq").unwrap();
        let cpath =
            std::ffi::CString::new(tree.join("bin").as_os_str().as_encoded_bytes()).unwrap();
        let name = std::ffi::CString::new("trusted.overlay.opaque").unwrap();
        let mut value = [0_u8; 8];
        // SAFETY: buffers and C strings are valid for this getxattr call.
        let len = unsafe {
            libc::getxattr(
                cpath.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert!(len >= 0);
        assert_eq!(&value[..usize::try_from(len).unwrap()], b"y");
        assert!(tree.join("keep").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires skopeo, composefs/OverlayFS mounts, fs-verity, and a privileged host mount namespace"]
    async fn oci_rootfs_modes_share_verified_layers_and_keep_writes_isolated() {
        let test_id = ulid::Ulid::generate().to_string();
        let base = PathBuf::from("/var/lib/odorobo/oci-rootfs-tests").join(&test_id);
        let (layout, _) = fixture_oci_layout_with_fill(&base.join("base"), b'x');
        let (changed_layout, _) = fixture_oci_layout_with_fill(&base.join("changed"), b'y');
        let image = format!("oci:{}:latest", layout.display());
        let changed_image = format!("oci:{}:latest", changed_layout.display());
        let ephemeral_a = ulid::Ulid::generate().to_string();
        let ephemeral_b = ulid::Ulid::generate().to_string();
        let readonly_a = ulid::Ulid::generate().to_string();
        let readonly_b = ulid::Ulid::generate().to_string();
        let persistent = ulid::Ulid::generate().to_string();

        let a = super::mount_rootfs(
            &ephemeral_a,
            &crate::manifest::Rootfs {
                oci: image.clone(),
                mode: crate::manifest::RootfsMode::Ephemeral,
            },
        )
        .await
        .unwrap();
        let b = super::mount_rootfs(
            &ephemeral_b,
            &crate::manifest::Rootfs {
                oci: image.clone(),
                mode: crate::manifest::RootfsMode::Ephemeral,
            },
        )
        .await
        .unwrap();
        std::fs::write(a.mount.join("etc/only-a"), b"a").unwrap();
        assert!(
            !b.mount.join("etc/only-a").exists(),
            "ephemeral upper must be VM-private"
        );
        super::unmount_rootfs(&a).await.unwrap();
        super::unmount_rootfs(&b).await.unwrap();
        drop(a);
        drop(b);
        let a = super::mount_rootfs(
            &ephemeral_a,
            &crate::manifest::Rootfs {
                oci: image.clone(),
                mode: crate::manifest::RootfsMode::Ephemeral,
            },
        )
        .await
        .unwrap();
        assert!(
            !a.mount.join("etc/only-a").exists(),
            "ephemeral writes must disappear at stop"
        );
        super::unmount_rootfs(&a).await.unwrap();
        drop(a);

        let ro_a = super::mount_rootfs(
            &readonly_a,
            &crate::manifest::Rootfs {
                oci: image.clone(),
                mode: crate::manifest::RootfsMode::ReadOnly,
            },
        )
        .await
        .unwrap();
        let ro_b = super::mount_rootfs(
            &readonly_b,
            &crate::manifest::Rootfs {
                oci: image.clone(),
                mode: crate::manifest::RootfsMode::ReadOnly,
            },
        )
        .await
        .unwrap();
        std::fs::write(ro_a.mount.join("tmp/private"), b"scratch").unwrap();
        assert!(
            !ro_b.mount.join("tmp/private").exists(),
            "read-only scratch must be private per VM"
        );
        assert!(
            std::fs::write(ro_a.mount.join("etc/denied"), b"no").is_err(),
            "read-only root writes outside scratch must fail"
        );
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        for path in &ro_a.scratch_mounts {
            let record = mountinfo
                .lines()
                .find(|line| line.split_whitespace().nth(4) == path.to_str())
                .unwrap();
            let flags = record.split_whitespace().nth(5).unwrap();
            assert!(flags.split(',').any(|flag| flag == "nosuid"));
            assert!(flags.split(',').any(|flag| flag == "nodev"));
        }
        assert!(
            !ro_a.mount.join("etc/deleted").exists(),
            "single-layer read-only root must interpret whiteouts"
        );
        super::unmount_rootfs(&ro_a).await.unwrap();
        super::unmount_rootfs(&ro_b).await.unwrap();
        drop(ro_a);
        drop(ro_b);

        // A late missing scratch path must fail without leaving a /tmp
        // tmpfs, the root bind, or its composefs layer mounted.
        let (incomplete_layout, _) =
            fixture_oci_layout_with_options(&base.join("missing-scratch"), b'z', false);
        let incomplete_ref = format!("oci:{}:latest", incomplete_layout.display());
        let failed_scratch_vmid = ulid::Ulid::generate().to_string();
        assert!(
            super::mount_rootfs(
                &failed_scratch_vmid,
                &crate::manifest::Rootfs {
                    oci: incomplete_ref,
                    mode: crate::manifest::RootfsMode::ReadOnly,
                }
            )
            .await
            .is_err()
        );
        let failed_runtime = crate::ch_driver::VMInstance::runtime_dir_for(&failed_scratch_vmid);
        assert!(
            !super::is_mountpoint(&failed_runtime.join(super::ROOTFS_TAG))
                .await
                .unwrap()
        );
        let layer_root = failed_runtime.join("rootfs-layers");
        for entry in std::fs::read_dir(&layer_root).unwrap() {
            assert!(!super::is_mountpoint(&entry.unwrap().path()).await.unwrap());
        }

        let p = super::mount_rootfs(
            &persistent,
            &crate::manifest::Rootfs {
                oci: image.clone(),
                mode: crate::manifest::RootfsMode::Persistent,
            },
        )
        .await
        .unwrap();
        std::fs::write(p.mount.join("etc/survives"), b"persist").unwrap();
        super::unmount_rootfs(&p).await.unwrap();
        drop(p);
        assert!(
            super::mount_rootfs(
                &persistent,
                &crate::manifest::Rootfs {
                    oci: changed_image,
                    mode: crate::manifest::RootfsMode::Persistent
                }
            )
            .await
            .is_err(),
            "changing the base digest must be rejected"
        );
        let p = super::mount_rootfs(
            &persistent,
            &crate::manifest::Rootfs {
                oci: image,
                mode: crate::manifest::RootfsMode::Persistent,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(p.mount.join("etc/survives")).unwrap(),
            b"persist"
        );
        super::unmount_rootfs(&p).await.unwrap();
        drop(p);
        super::delete_persistent_rootfs(&persistent).await.unwrap();
        for vmid in [
            ephemeral_a,
            ephemeral_b,
            readonly_a,
            readonly_b,
            failed_scratch_vmid,
            persistent,
        ] {
            _ = std::fs::remove_dir_all(crate::ch_driver::VMInstance::runtime_dir_for(&vmid));
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires privileged composefs mounts; failed scratch setup retains resource owner"]
    async fn failed_rootfs_start_retains_mount_inventory_for_retry() {
        let base = Path::new("/var/lib/odorobo/startup-owner-tests")
            .join(ulid::Ulid::generate().to_string());
        let (layout, _) = fixture_oci_layout_with_options(&base, b'x', false);
        let vmid = ulid::Ulid::generate().to_string();
        let mut owner = None;
        let rootfs = crate::manifest::Rootfs {
            oci: format!("oci:{}:latest", layout.display()),
            mode: crate::manifest::RootfsMode::ReadOnly,
        };
        assert!(
            super::mount_rootfs_owned(&vmid, &rootfs, &mut owner)
                .await
                .is_err()
        );
        let prepared = owner
            .as_ref()
            .expect("failed acquisition must retain ownership");
        assert!(super::is_mountpoint(&prepared.mount).await.unwrap());
        assert!(super::acquire_rootfs_lock(&vmid).await.is_err());
        let layer = &prepared.layer_mounts[0];
        let mut busy = tokio::process::Command::new("sleep")
            .arg("100")
            .current_dir(layer)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        assert!(super::unmount_rootfs(prepared).await.is_err());
        assert!(super::acquire_rootfs_lock(&vmid).await.is_err());
        busy.kill().await.unwrap();
        super::unmount_rootfs(prepared).await.unwrap();
        drop(owner);
        assert!(super::acquire_rootfs_lock(&vmid).await.is_ok());
        std::fs::remove_dir_all(crate::ch_driver::VMInstance::runtime_dir_for(&vmid)).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires privileged mounts and host composefs/fs-verity kernel support"]
    async fn composefs_layer_mount_verifies_image_and_backing_files() {
        let base = PathBuf::from("/var/lib/odorobo/composefs-mount-test")
            .join(ulid::Ulid::generate().to_string());
        let (layout, layer) = fixture_oci_layout(&base);
        let cache = super::materialize_layer_at(&layout, &layer, &base.join("cache"))
            .await
            .unwrap();
        let mount = base.join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        let digest = std::fs::read_to_string(cache.join("layer-digest")).unwrap();
        let options = format!(
            "basedir={},digest={},verity,ro",
            cache.join("store").display(),
            digest.trim()
        );
        let mounted = tokio::process::Command::new("mount.composefs")
            .args(["-o", &options])
            .arg(cache.join("layer.cfs"))
            .arg(&mount)
            .output()
            .await
            .unwrap();
        assert!(
            mounted.status.success(),
            "{}",
            String::from_utf8_lossy(&mounted.stderr)
        );
        assert_eq!(
            std::fs::read(mount.join("etc/x")).unwrap(),
            vec![b'x'; 16 * 1024]
        );
        let unmounted = tokio::process::Command::new("umount")
            .arg(&mount)
            .output()
            .await
            .unwrap();
        assert!(
            unmounted.status.success(),
            "{}",
            String::from_utf8_lossy(&unmounted.stderr)
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires CAP_SYS_ADMIN to mount OverlayFS"]
    async fn overlay_lower_order_and_whiteout_visibility() {
        let base =
            PathBuf::from("/var/lib/odorobo-overlay-test").join(ulid::Ulid::generate().to_string());
        let lower = base.join("base-layer");
        let upper_layer = base.join("top-layer");
        let merged = base.join("merged");
        std::fs::create_dir_all(lower.join("etc/opaque")).unwrap();
        std::fs::create_dir_all(upper_layer.join("etc/opaque")).unwrap();
        std::fs::create_dir_all(&merged).unwrap();
        std::fs::write(lower.join("etc/deleted"), b"old").unwrap();
        std::fs::write(lower.join("etc/replaced"), b"old").unwrap();
        std::fs::write(lower.join("etc/opaque/old"), b"old").unwrap();
        std::fs::write(upper_layer.join("etc/replaced"), b"top").unwrap();
        apply_whiteout(&upper_layer, Path::new("etc"), ".wh.deleted").unwrap();
        apply_whiteout(&upper_layer, Path::new("etc/opaque"), ".wh..wh..opq").unwrap();
        std::fs::write(upper_layer.join("etc/opaque/new"), b"new").unwrap();
        let options = format!("lowerdir={}:{},ro", upper_layer.display(), lower.display());
        let mounted = std::process::Command::new("mount")
            .args(["-t", "overlay", "overlay", "-o", &options])
            .arg(&merged)
            .output()
            .unwrap();
        assert!(
            mounted.status.success(),
            "{}",
            String::from_utf8_lossy(&mounted.stderr)
        );
        let result = (|| {
            assert!(!merged.join("etc/deleted").exists());
            assert_eq!(std::fs::read(merged.join("etc/replaced"))?, b"top");
            assert!(!merged.join("etc/opaque/old").exists());
            assert_eq!(std::fs::read(merged.join("etc/opaque/new"))?, b"new");
            Ok::<(), std::io::Error>(())
        })();
        let unmounted = std::process::Command::new("umount")
            .arg(&merged)
            .output()
            .unwrap();
        std::fs::remove_dir_all(base).unwrap();
        assert!(
            unmounted.status.success(),
            "{}",
            String::from_utf8_lossy(&unmounted.stderr)
        );
        result.unwrap();
    }

    #[test]
    fn persistent_rbd_lookup_matches_only_default_namespace_head() {
        let mappings = serde_json::json!([
            {"pool":"pool", "namespace":"foo", "name":"volume", "device":"/dev/rbd0"},
            {"pool":"pool", "namespace":"", "name":"volume", "snap":"snapshot", "device":"/dev/rbd1"},
            {"pool":"pool", "namespace":"", "name":"volume", "snap":"-", "device":"/dev/rbd2"}
        ]);
        assert_eq!(
            super::match_rbd_head_device("pool/volume", &mappings).unwrap(),
            Some("/dev/rbd2".into())
        );
        assert!(
            super::match_rbd_head_device("pool/volume", &serde_json::json!([mappings[0].clone()]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn hard_links_preserve_same_layer_content_and_reject_missing_or_escaping_targets() {
        for target in ["file", "base-only", "../escape"] {
            let base =
                std::env::temp_dir().join(format!("odorobo-hardlink-{}", ulid::Ulid::generate()));
            std::fs::create_dir_all(&base).unwrap();
            let mut bytes = Vec::new();
            let mut tar = tar::Builder::new(&mut bytes);
            let mut header = tar::Header::new_gnu();
            header.set_uid(1234);
            header.set_gid(2345);
            header.set_mode(0o640);
            header.set_size(4);
            header.set_cksum();
            tar.append_data(&mut header, "file", &b"data"[..]).unwrap();
            header.set_entry_type(tar::EntryType::Link);
            header.set_size(0);
            header.set_link_name(target).unwrap();
            header.set_cksum();
            tar.append_data(&mut header, "alias", std::io::empty())
                .unwrap();
            tar.finish().unwrap();
            drop(tar);
            let result =
                super::unpack_layer(bytes.as_slice(), Compression::None, &base, 1024 * 1024, 10);
            if target == "file" {
                result.unwrap();
                assert_eq!(std::fs::read(base.join("alias")).unwrap(), b"data");
                assert_eq!(
                    std::fs::metadata(base.join("file")).unwrap().ino(),
                    std::fs::metadata(base.join("alias")).unwrap().ino()
                );
            } else {
                assert!(result.is_err());
            }
            std::fs::remove_dir_all(base).unwrap();
        }
    }

    #[test]
    fn directory_metadata_does_not_cross_replacement_generations_or_symlinks() {
        let base =
            std::env::temp_dir().join(format!("odorobo-generation-{}", ulid::Ulid::generate()));
        let layers = [base.join("a"), base.join("b"), base.join("c")];
        for layer in &layers {
            std::fs::create_dir_all(layer).unwrap();
        }
        std::fs::create_dir_all(layers[0].join("app/private")).unwrap();
        std::fs::create_dir_all(layers[2].join("app/private")).unwrap();
        let changes = [vec!["app/private".into()], vec![], vec!["app".into()]];
        let path = Path::new("app/private");
        assert_eq!(
            super::directory_metadata_source(
                path,
                &layers,
                &changes,
                &[
                    vec![],
                    vec![super::DirectoryReset {
                        path: "app".into(),
                        descendants_only: false
                    }],
                    vec![]
                ]
            )
            .unwrap(),
            2
        );
        std::fs::write(layers[1].join("app"), b"replacement").unwrap();
        assert_eq!(
            super::directory_metadata_source(path, &layers, &changes, &[vec![], vec![], vec![]])
                .unwrap(),
            2
        );
        std::fs::remove_file(layers[1].join("app")).unwrap();
        std::os::unix::fs::symlink("/etc", layers[1].join("app")).unwrap();
        assert_eq!(
            super::directory_metadata_source(path, &layers, &changes, &[vec![], vec![], vec![]])
                .unwrap(),
            2
        );
        std::fs::remove_file(layers[1].join("app")).unwrap();
        std::fs::create_dir_all(layers[1].join("app/private")).unwrap();
        assert_eq!(
            super::directory_metadata_source(
                path,
                &layers,
                &changes,
                &[
                    vec![],
                    vec![super::DirectoryReset {
                        path: "app".into(),
                        descendants_only: false
                    }],
                    vec![]
                ]
            )
            .unwrap(),
            1
        );
        let opaque = [
            vec![],
            vec![super::DirectoryReset {
                path: "app".into(),
                descendants_only: true,
            }],
            vec![],
        ];
        assert_eq!(
            super::directory_metadata_source(
                Path::new("app"),
                &layers,
                &[vec!["app".into()], vec![], vec![]],
                &opaque
            )
            .unwrap(),
            0
        );
        assert_eq!(
            super::directory_metadata_source(path, &layers, &changes, &opaque).unwrap(),
            1
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires composefs/OverlayFS mounts and fs-verity"]
    async fn synthetic_layer_parents_preserve_inherited_directory_metadata() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        let vmid = ulid::Ulid::generate().to_string();
        let runtime = crate::ch_driver::VMInstance::runtime_dir_for(&vmid);
        let base = runtime.join("base");
        let top = runtime.join("top");
        let merged = runtime.join("merged");
        for path in [&base, &top, &merged] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::create_dir(base.join("app")).unwrap();
        std::fs::create_dir(top.join("app")).unwrap();
        std::fs::write(base.join("app/old"), b"old").unwrap();
        std::fs::write(top.join("app/new"), b"new").unwrap();
        for path in [&base, &base.join("app")] {
            let file = std::fs::File::open(path).unwrap();
            // SAFETY: live fd; disposable test metadata.
            assert_eq!(unsafe { libc::fchown(file.as_raw_fd(), 1000, 1000) }, 0);
            file.set_permissions(std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let correction = super::compose_directory_metadata(
            &runtime,
            &merged,
            &[base.clone(), top.clone()],
            &[vec![PathBuf::new(), "app".into()], vec![]],
            &[vec![], vec![]],
        )
        .await
        .unwrap();
        let options = format!(
            "ro,lowerdir={}:{}:{}",
            correction.display(),
            top.display(),
            base.display()
        );
        super::run_checked(
            "mount",
            [
                "-t".into(),
                "overlay".into(),
                "overlay".into(),
                "-o".into(),
                options,
            ],
            &[&merged],
        )
        .await
        .unwrap();
        for path in [&merged, &merged.join("app")] {
            let metadata = std::fs::metadata(path).unwrap();
            assert_eq!(
                (metadata.uid(), metadata.gid(), metadata.mode() & 0o7777),
                (1000, 1000, 0o700)
            );
        }
        assert_eq!(std::fs::read(merged.join("app/new")).unwrap(), b"new");
        assert_eq!(std::fs::read(merged.join("app/old")).unwrap(), b"old");
        super::unmount_path(&merged).await.unwrap();
        super::unmount_path(&correction).await.unwrap();
        std::fs::remove_dir_all(&runtime).unwrap();
        std::fs::remove_dir_all(Path::new("/var/lib/odorobo/directory-metadata").join(vmid))
            .unwrap();
    }

    /// Exercises the product's Ceph RBD persistence helpers against a unique
    /// disposable image. Run only in the privileged `.local/dev` Odorobo
    /// container with ODOROBO_ROOTFS_BACKEND=rbd and a small test size.
    #[tokio::test]
    #[ignore = "requires privileged RBD access; uses and deletes a uniquely named disposable image"]
    async fn rbd_persistent_upper_cold_reattach_and_delete() {
        assert_eq!(
            std::env::var("ODOROBO_ROOTFS_BACKEND").as_deref(),
            Ok("rbd")
        );
        let vmid = ulid::Ulid::generate().to_string();
        let digest = format!("sha256:{}", "a".repeat(64));
        let lock = super::acquire_rootfs_lock(&vmid).await.unwrap();
        let first = super::prepare_upper(
            &vmid,
            crate::manifest::RootfsMode::Persistent,
            &digest,
            Path::new("/run/odorobo/test-runtime"),
        )
        .await
        .unwrap();
        let upper = first.upper.as_ref().unwrap();
        std::fs::write(upper.join("persistent-marker"), b"survives reattach").unwrap();
        let image = first.rbd_image.clone().unwrap();
        super::release_persistent_backend(first.state_mount.as_deref(), first.rbd_image.as_deref())
            .await
            .unwrap();
        drop(first);
        drop(lock);

        let lock = super::acquire_rootfs_lock(&vmid).await.unwrap();
        let second = super::prepare_upper(
            &vmid,
            crate::manifest::RootfsMode::Persistent,
            &digest,
            Path::new("/run/odorobo/test-runtime"),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(second.upper.as_ref().unwrap().join("persistent-marker")).unwrap(),
            b"survives reattach"
        );
        super::release_persistent_backend(
            second.state_mount.as_deref(),
            second.rbd_image.as_deref(),
        )
        .await
        .unwrap();
        drop(second);
        drop(lock);
        super::delete_persistent_rootfs(&vmid).await.unwrap();
        let info = super::rbd_output(&["info".into(), image]).await.unwrap();
        assert!(
            !info.status.success(),
            "explicit delete must remove the per-VM RBD image"
        );
    }

    #[tokio::test]
    async fn supervisor_restarts_dead_processes_and_stops_cleanly() {
        let base = std::env::temp_dir().join(format!("odorobo-sup-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base).unwrap();
        let supervisor = VirtioFsSupervisor::start(VirtiofsdSpec {
            // /bin/false exits instantly -> the supervisor should keep
            // restarting it with backoff.
            program: "/bin/false".into(),
            socket: base.join("sock"),
            shared_dir: base.clone(),
            log: base.join("log"),
        })
        .expect("supervisor starts");

        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            supervisor.restarts() >= 1,
            "dead process should be restarted"
        );
        assert!(!base.join("sock").exists(), "no socket from /bin/false");

        supervisor.stop().await;
        let restarts_at_stop = supervisor.restarts();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            supervisor.restarts(),
            restarts_at_stop,
            "no restarts after stop"
        );

        // Use a script that accepts virtiofsd's arguments and execs a real
        // long-running child. /bin/sleep rejects those arguments immediately.
        let base2 = std::env::temp_dir().join(format!("odorobo-sup2-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base2).unwrap();
        let script = base2.join("virtiofsd-stub");
        std::fs::write(
            &script,
            "#!/bin/sh\necho $$ > \"$(dirname \"$0\")/pid\"\nexec sleep 100\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let supervisor = VirtioFsSupervisor::start(VirtiofsdSpec {
            program: script.clone(),
            socket: base2.join("sock"),
            shared_dir: base2.clone(),
            log: base2.join("log"),
        })
        .expect("supervisor starts");
        tokio::time::sleep(Duration::from_millis(300)).await;
        supervisor.stop().await;
        let pid: u32 = std::fs::read_to_string(base2.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child leaked after stop"
        );
        let supervisor = VirtioFsSupervisor::start(VirtiofsdSpec {
            program: script,
            socket: base2.join("sock"),
            shared_dir: base2.clone(),
            log: base2.join("log"),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let pid: u32 = std::fs::read_to_string(base2.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(Path::new(&format!("/proc/{pid}")).exists());
        drop(supervisor);
        for _ in 0..100 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child leaked after drop"
        );

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&base2).ok();
    }
}
