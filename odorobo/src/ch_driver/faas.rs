//! FaaS microVM rootfs provisioning (issue #112): OCI images served as the
//! guest root over virtiofs, backed by digest-pinned composefs mounts.
//!
//! Node-side pipeline (composefs lives entirely on the node; the guest sees a
//! plain virtiofs root and boots `root=rootfs rootfstype=virtiofs` with no
//! initramfs):
//!
//! 1. `skopeo copy docker://<ref> oci:<cache>/<slug>:rootfs` — the pulled
//!    OCI layout is cached once per image reference under
//!    `/var/lib/odorobo/oci-cache`.
//! 2. Unpack layers (tar, gzip/zstd, OCI whiteouts, per-layer sha256
//!    verification) into a scratch tree.
//! 3. `mkcomposefs --digest-store=<store> <tree> root.cfs` — a
//!    content-addressed object store plus a small composefs (erofs) image.
//!    The scratch tree is removed afterwards: the store holds all content.
//! 4. `fsverity enable` the image; its fs-verity digest (measured with
//!    `composefs-info measure-file`) keys the shared entry under
//!    `/var/lib/odorobo/roots/<digest>` — every VM running the same image
//!    dedupes on disk.
//! 5. Per VM: `mount.composefs` the digest-pinned image at
//!    `<runtime>/rootfs` (read-only and shared, or with a per-VM overlayfs
//!    upper directory when the manifest asks for a writable root), then run
//!    virtiofsd serving that mount.
//!
//! Security notes: layers are unpacked by this process as root; symlink
//! targets inside images are taken as-is (they resolve inside the guest
//! view). The store must live on an fs-verity-capable filesystem (ext4,
//! btrfs) for digest-pinned mounts.

use std::io::Read;
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

use crate::manifest::Rootfs;

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
const ROOTS_ROOT: &str = "/var/lib/odorobo/roots";
const SKOPEO_TIMEOUT: Duration = Duration::from_secs(600);

/// A rootfs made ready for a specific VM.
#[derive(Debug, Clone)]
pub struct PreparedRootfs {
    /// fs-verity digest (hex) of the pinned composefs image.
    pub digest: String,
    /// Shared composefs image file, e.g. /var/lib/odorobo/roots/<digest>/root.cfs
    pub image: PathBuf,
    /// Shared content-addressed object store (composefs basedir).
    pub store: PathBuf,
    /// This VM's mount point of the rootfs (virtiofsd shared-dir).
    pub mount: PathBuf,
    /// Per-VM overlayfs upper directory when the root is writable.
    pub upper: Option<PathBuf>,
    /// Per-VM overlayfs work directory when the root is writable.
    pub work: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// OCI pull + unpack
// ---------------------------------------------------------------------------

/// Filesystem-safe slug for an image reference (cache dir / cache key).
fn sanitize_image_ref(image_ref: &str) -> String {
    image_ref
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Accepts bare docker refs and explicit transports alike.
fn normalize_image_ref(image_ref: &str) -> String {
    const TRANSPORTS: [&str; 5] =
        ["docker://", "oci:", "dir:", "containers-storage:", "docker-archive:"];
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

/// `Read` adapter that hashes every byte it yields. Placed *below* the
/// decompressor so the digest covers the compressed blob bytes, which is
/// what OCI layer digests refer to.
struct HashingTee<R: Read> {
    inner: R,
    hasher: Arc<std::sync::Mutex<sha2::Sha256>>,
}

impl<R: Read> Read for HashingTee<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Ok(mut hasher) = self.hasher.lock() {
            hasher.update(&buf[..n]);
        }
        Ok(n)
    }
}

fn digest_blob_path(layout: &Path, digest: &str) -> Result<PathBuf> {
    let (algorithm, hex) = digest
        .split_once(':')
        .ok_or_else(|| eyre!("malformed digest {digest:?}"))?;
    if algorithm != "sha256" {
        bail!("unsupported digest algorithm {algorithm:?}");
    }
    Ok(layout.join("blobs").join(algorithm).join(hex))
}

/// Pull the image reference into the per-ref OCI layout cache; returns the
/// layout dir. Cached pulls are reused as-is (digest-pinned world: content
/// never changes under a digest; tag drift means operators clear the cache).
async fn pull_oci(image_ref: &str) -> Result<PathBuf> {
    let normalized = normalize_image_ref(image_ref);
    let dir = PathBuf::from(OCI_CACHE_ROOT).join(sanitize_image_ref(image_ref));
    if dir.join("index.json").exists() {
        debug!(image_ref, dir = %dir.display(), "OCI layout cache hit");
        return Ok(dir);
    }
    tokio::fs::create_dir_all(&dir)
        .await
        .wrap_err_with(|| format!("create OCI cache dir {}", dir.display()))?;
    let tmp = dir.with_file_name(format!(".pull-{}", ulid::Ulid::generate()));
    let tag_ref = format!("oci:{}:rootfs", tmp.display());

    info!(image_ref, dir = %dir.display(), "pulling OCI image with skopeo");
    let output = tokio::time::timeout(
        SKOPEO_TIMEOUT,
        Command::new("skopeo")
            .args(["copy", "--remove-signatures", &normalized, &tag_ref])
            .output(),
    )
    .await
    .map_err(|_| eyre!("skopeo pull of {normalized} timed out after 600s"))?
    .wrap_err("failed to run skopeo (is it installed? `dnf install skopeo`)")?;

    if !output.status.success() {
        _ = tokio::fs::remove_dir_all(&tmp).await;
        bail!(
            "skopeo pull of {normalized} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    match tokio::fs::rename(&tmp, &dir).await {
        Ok(()) => Ok(dir),
        Err(err) if dir.join("index.json").exists() => {
            // Lost a race with another VM pulling the same ref; reuse theirs.
            debug!(?err, "losing pull race, reusing existing layout");
            _ = tokio::fs::remove_dir_all(&tmp).await;
            Ok(dir)
        }
        Err(err) => Err(eyre!("publish pulled layout to {}: {err}", dir.display())),
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

/// Apply an OCI whiteout entry (`.wh.<name>` / `.wh..wh..opk`) relative to
/// the layer's parent directory in the unpacked tree.
fn apply_whiteout(tree: &Path, parent_rel: &Path, file_name: &str) {
    if file_name == ".wh..wh..opk" {
        // Opaque whiteout: the parent directory becomes empty relative to
        // lower layers.
        let parent = tree.join(parent_rel);
        let Ok(entries) = std::fs::read_dir(&parent) else {
            return;
        };
        for entry in entries.flatten() {
            remove_path(&entry.path());
        }
        debug!(dir = %parent.display(), "applied opaque whiteout");
    } else {
        let target = file_name.strip_prefix(".wh.").unwrap_or(file_name);
        remove_path(&tree.join(parent_rel.join(target)));
        debug!(target, "applied whiteout");
    }
}

/// Unpack one tar entry into the tree, handling OCI whiteouts. Device and
/// fifo nodes are skipped (the guest kernel's devtmpfs provides /dev).
fn unpack_entry<R: Read>(tree: &Path, mut entry: tar::Entry<'_, R>) -> Result<()> {
    let raw_path = entry.path()?.to_path_buf();
    let Some(rel) = safe_rel_path(&raw_path) else {
        bail!("refusing to extract suspicious tar path {raw_path:?}");
    };
    let file_name = rel
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    if file_name.starts_with(".wh.") {
        let parent = rel.parent().unwrap_or(Path::new("")).to_path_buf();
        apply_whiteout(tree, &parent, &file_name);
        return Ok(());
    }

    match entry.header().entry_type() {
        tar::EntryType::Directory => {
            let target = tree.join(&rel);
            std::fs::create_dir_all(&target)
                .wrap_err_with(|| format!("mkdir {}", target.display()))?;
        }
        tar::EntryType::Regular | tar::EntryType::Symlink | tar::EntryType::Link => {
            if let Some(parent) = tree.join(&rel).parent() {
                std::fs::create_dir_all(parent)?;
            }
            // unpack_in() extracts at the entry's (already validated) path
            // and handles contents, permissions, symlinks and hard links.
            entry
                .unpack_in(tree)
                .wrap_err_with(|| format!("extract {}", rel.display()))?;
        }
        other => {
            warn!(entry = %rel.display(), ?other, "skipping unsupported tar entry type");
        }
    }
    Ok(())
}

fn extract_entries<R: Read>(mut archive: tar::Archive<R>, tree: &Path) -> Result<()> {
    for entry in archive.entries().wrap_err("iterate tar entries")? {
        unpack_entry(tree, entry.wrap_err("tar entry")?)?;
    }
    Ok(())
}

/// Extract a layer blob with digest verification; returns the `sha256:`
/// digest of the compressed blob that was extracted.
fn digest_verified_unpack<R: Read>(
    reader: R,
    compression: Compression,
    tree: &Path,
) -> Result<String> {
    let hasher = Arc::new(std::sync::Mutex::new(sha2::Sha256::new()));
    let tee = HashingTee { inner: reader, hasher: Arc::clone(&hasher) };

    match compression {
        Compression::None => extract_entries(tar::Archive::new(tee), tree)?,
        Compression::Gzip => {
            extract_entries(tar::Archive::new(flate2::read::GzDecoder::new(tee)), tree)?
        }
        Compression::Zstd => extract_entries(
            tar::Archive::new(zstd::stream::read::Decoder::new(tee).wrap_err("zstd decoder init")?),
            tree,
        )?,
    }

    let hex = {
        // Clone the hasher so we can consume the clone with `finalize`
        // (Digest::finalize takes self by value) without losing the running
        // state.
        let hasher = hasher
            .lock()
            .map_err(|_| eyre!("layer hasher lock poisoned"))?;
        to_hex(&hasher.clone().finalize())
    };
    Ok(format!("sha256:{hex}"))
}

/// Unpack all layers of an OCI layout into `tree`, in order, verifying each
/// layer blob against its descriptor digest.
fn unpack_oci_layers_sync(layout: &Path, tree: &Path) -> Result<()> {
    std::fs::create_dir_all(tree)?;
    let index: serde_json::Value = serde_json::from_reader(
        std::fs::File::open(layout.join("index.json"))
            .wrap_err(format!("open index.json in {}", layout.display()))?,
    )
    .wrap_err("parse OCI index.json")?;
    let manifest_digest = index["manifests"]
        .as_array()
        .and_then(|manifests| manifests.first())
        .and_then(|manifest| manifest["digest"].as_str())
        .ok_or_else(|| eyre!("OCI index has no manifest digest"))?
        .to_owned();
    let manifest_path = digest_blob_path(layout, &manifest_digest)?;
    let manifest: serde_json::Value = serde_json::from_reader(
        std::fs::File::open(&manifest_path)
            .wrap_err_with(|| format!("open manifest {}", manifest_path.display()))?,
    )
    .wrap_err("parse OCI image manifest")?;
    let layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| eyre!("OCI manifest has no layers"))?
        .to_vec();

    for (index, layer) in layers.iter().enumerate() {
        let media_type = layer["mediaType"].as_str().unwrap_or_default();
        let Some(compression) = compression_of_media_type(media_type) else {
            bail!("unsupported layer media type {media_type:?} (layer {index})");
        };
        let digest = layer["digest"]
            .as_str()
            .ok_or_else(|| eyre!("layer {index} has no digest"))?
            .to_owned();
        let blob = digest_blob_path(layout, &digest)?;

        debug!(layer = index, %digest, ?compression, "unpacking layer");
        let file = std::fs::File::open(&blob)
            .wrap_err_with(|| format!("open layer blob {}", blob.display()))?;
        let expected = digest.clone();
        let actual = digest_verified_unpack(
            std::io::BufReader::new(file),
            compression,
            tree,
        )?;
        if actual != expected {
            bail!("layer {index} digest mismatch: expected {expected}, content is {actual}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// composefs build + mount
// ---------------------------------------------------------------------------

fn roots_root() -> PathBuf {
    PathBuf::from(ROOTS_ROOT)
}

/// vhost-user socket path for a VM's rootfs device. Single source of truth
/// shared with the manifest conversion's FsConfig so the actor's virtiofsd
/// supervisor and the CH fs device config always agree.
pub fn socket_path_for(vmid: &str) -> PathBuf {
    crate::ch_driver::VMInstance::runtime_dir_for(vmid).join(format!("{ROOTFS_TAG}.sock"))
}

/// (image, store, fs-verity digest) built from an unpacked tree. The result
/// is published under roots/<digest>; a losing racer reuses the winner's
/// entry.
async fn build_composefs_entry(
    tree: &Path,
    image_ref_slug: &str,
) -> Result<(PathBuf, PathBuf, String)> {
    let roots = roots_root();
    tokio::fs::create_dir_all(&roots)
        .await
        .wrap_err_with(|| format!("create {}", roots.display()))?;
    let tmp = roots.join(format!(".build-{}", ulid::Ulid::generate()));
    tokio::fs::create_dir_all(&tmp).await?;
    let store = tmp.join("store");
    let image = tmp.join("root.cfs");

    debug!(tree = %tree.display(), "building composefs image");
    run_checked(
        "mkcomposefs",
        [
            "--digest-store".to_owned(),
            store.display().to_string(),
            tree.display().to_string(),
            image.display().to_string(),
        ],
        &[],
    )
    .await
    .wrap_err("mkcomposefs failed")?;

    run_checked(
        "fsverity",
        ["enable".to_owned()],
        &[image.as_path()],
    )
    .await
    .wrap_err("fsverity enable on composefs image failed (backing fs must support fs-verity)")?;

    let digest = run_checked(
        "composefs-info",
        ["measure-file".to_owned()],
        &[image.as_path()],
    )
    .await
    .wrap_err("measuring composefs image digest failed")?;
    let digest = digest.trim().to_owned();

    let entry = roots.join(&digest);
    if entry.exists() {
        // Another VM already published this exact root; drop our duplicate.
        tokio::fs::remove_dir_all(&tmp).await.ok();
    } else {
        // Persist the source ref for cache reuse across VMs.
        let marker = tmp.join("ref");
        tokio::fs::write(&marker, format!("{image_ref_slug}\n"))
            .await
            .ok();
        match tokio::fs::rename(&tmp, &entry).await {
            Ok(()) => {}
            Err(err) if entry.exists() => {
                debug!(?err, "losing composefs publish race, reusing winner");
                tokio::fs::remove_dir_all(&tmp).await.ok();
            }
            Err(err) => bail!("publish composefs entry {}: {err}", entry.display()),
        }
    }

    Ok((entry.join("root.cfs"), entry.join("store"), digest))
}

/// Existing roots entry previously built for this image ref slug, if any.
async fn find_root_entry(image_ref_slug: &str) -> Option<PathBuf> {
    let mut entries = tokio::fs::read_dir(roots_root()).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if let Ok(marker) = tokio::fs::read_to_string(entry.path().join("ref")).await {
            if marker.trim() == image_ref_slug {
                return Some(entry.path());
            }
        }
    }
    None
}

/// Prepare and mount the OCI rootfs for one VM. The composefs store/image are
/// shared per digest; the mount (and the overlay upper when writable) is
/// per-VM.
pub async fn mount_rootfs(vmid: &str, rootfs: &Rootfs) -> Result<PreparedRootfs> {
    let slug = sanitize_image_ref(&rootfs.oci);

    let (image, store, digest) = if let Some(entry) = find_root_entry(&slug).await {
        info!(
            vmid,
            image_ref = %rootfs.oci,
            entry = %entry.display(),
            "composefs root cache hit"
        );
        (
            entry.join("root.cfs"),
            entry.join("store"),
            entry
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        let layout = pull_oci(&rootfs.oci).await?;
        let scratch = roots_root().join(format!(".unpack-{}", ulid::Ulid::generate()));
        let unpack_tree = scratch.join("tree");
        let built = {
            let layout = layout.clone();
            let tree = unpack_tree.clone();
            let unpack = tokio::task::spawn_blocking(move || -> Result<()> {
                unpack_oci_layers_sync(&layout, &tree)
            })
            .await
            .map_err(|err| eyre!("layer unpack task panicked: {err}"))?;
            unpack?;
            build_composefs_entry(&unpack_tree, &slug).await
        };
        tokio::fs::remove_dir_all(&scratch).await.ok();
        built?
    };

    // Per-VM mount of the pinned image.
    let runtime_dir = crate::ch_driver::VMInstance::runtime_dir_for(vmid);
    let mount = runtime_dir.join(ROOTFS_TAG);
    tokio::fs::create_dir_all(&mount)
        .await
        .wrap_err_with(|| format!("create mount point {}", mount.display()))?;

    let mut opts =
        vec![format!("basedir={}", store.display()), format!("digest={digest}")];
    let (upper, work) = if rootfs.read_only {
        (None, None)
    } else {
        let upper = runtime_dir.join("rootfs.upper");
        let work = runtime_dir.join("rootfs.work");
        for dir in [&upper, &work] {
            tokio::fs::create_dir_all(dir)
                .await
                .wrap_err_with(|| format!("create {}", dir.display()))?;
        }
        opts.push(format!("upperdir={}", upper.display()));
        opts.push(format!("workdir={}", work.display()));
        (Some(upper), Some(work))
    };

    run_checked(
        "mount.composefs",
        ["-o".to_owned(), opts.join(",")],
        &[image.as_path(), mount.as_path()],
    )
    .await
    .wrap_err_with(|| {
        format!("composefs mount of digest {digest} at {} failed", mount.display())
    })?;
    info!(
        vmid,
        %digest,
        mount = %mount.display(),
        writable = upper.is_some(),
        "rootfs mounted"
    );

    Ok(PreparedRootfs { digest, image, store, mount, upper, work })
}

/// Unmount a VM's rootfs and drop its per-VM overlay dirs. Best effort:
/// failures are logged and do not block teardown.
pub async fn unmount_rootfs(prepared: &PreparedRootfs) {
    for attempt in 1..=3 {
        let output = Command::new("umount").arg(&prepared.mount).output().await;
        match output {
            Ok(out) if out.status.success() => break,
            other => {
                debug!(attempt, ?other, "umount retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    if let Some(upper) = &prepared.upper {
        tokio::fs::remove_dir_all(upper).await.ok();
    }
    if let Some(work) = &prepared.work {
        tokio::fs::remove_dir_all(work).await.ok();
    }
    // The mount point dir itself lives in the VM runtime dir, which
    // purge_instance_data removes.
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
    let output = cmd
        .output()
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

// ---------------------------------------------------------------------------
// virtiofsd supervision (runs inside the VM actor)
// ---------------------------------------------------------------------------

/// Resolve the virtiofsd binary (Fedora ships it in /usr/libexec, not PATH).
pub fn virtiofsd_path() -> Option<PathBuf> {
    if let Ok(output) = std::process::Command::new("which").arg("virtiofsd").output() {
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
}

/// Runs virtiofsd as a supervised child of the VM actor for the VM's
/// lifetime: restarts with capped backoff on unexpected exit, stopped with
/// the actor. Children are `kill_on_drop`, so even an abrupt actor kill does
/// not leak the process.
#[derive(Clone)]
pub struct VirtioFsSupervisor {
    spec: VirtiofsdSpec,
    state: Arc<SupState>,
    task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
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
        });
        let task_state = Arc::clone(&state);
        let task_spec = spec.clone();

        let handle = tokio::spawn(async move {
            let mut backoff_secs: u64 = 1;
            loop {
                if task_state.shutdown.load(Ordering::Relaxed) {
                    break;
                }
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
                ])
                .stdin(std::process::Stdio::null())
                .stdout(log.try_clone().expect("log handle clone"))
                .stderr(log)
                .kill_on_drop(true);

                debug!(program = %task_spec.program.display(), "spawning virtiofsd");
                match cmd.spawn() {
                    Ok(mut child) => {
                        let exit = tokio::select! {
                            status = child.wait() => status,
                            _ = task_state.notify.notified() => {
                                // stop() asked us to go down; make sure the
                                // child is gone before exiting the loop.
                                _ = child.start_kill();
                                _ = child.wait().await;
                                break;
                            }
                        };
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
                warn!(restarts, backoff_secs, "virtiofsd backing off before restart");
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
            task: Arc::new(Mutex::new(Some(handle))),
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
            if self.spec.socket.exists() {
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
        self.state.shutdown.store(true, Ordering::Relaxed);
        self.state.notify.notify_one();
        if let Some(task) = self.task.lock().await.take() {
            // Give the loop a moment to observe the shutdown flag and reap
            // the child; abort (kill_on_drop) as a backstop.
            if tokio::time::timeout(Duration::from_secs(5), task).await.is_err() {
                warn!("virtiofsd supervisor task did not exit in time; aborting");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Compression, VirtioFsSupervisor, VirtiofsdSpec, apply_whiteout, compression_of_media_type,
        safe_rel_path, sanitize_image_ref,
    };
    use std::path::Path;
    use std::time::Duration;

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
        assert_eq!(compression_of_media_type("application/vnd.oci.image.index"), None);
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
    fn whiteouts_remove_files_and_clear_dirs() {
        let base = std::env::temp_dir().join(format!("odorobo-whiteout-{}", ulid::Ulid::generate()));
        let tree = base.join("tree");
        std::fs::create_dir_all(tree.join("bin")).unwrap();
        std::fs::write(tree.join("bin/ls"), b"old").unwrap();
        std::fs::write(tree.join("keep"), b"keep").unwrap();

        // plain whiteout removes bin/ls
        apply_whiteout(&tree, Path::new("bin"), ".wh.ls");
        assert!(!tree.join("bin/ls").exists());

        // opaque whiteout empties the whole bin dir
        std::fs::write(tree.join("bin/cat"), b"x").unwrap();
        apply_whiteout(&tree, Path::new("bin"), ".wh..wh..opk");
        assert!(tree.join("bin").read_dir().unwrap().next().is_none());
        assert!(tree.join("keep").exists());

        std::fs::remove_dir_all(&base).unwrap();
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
        assert!(supervisor.restarts() >= 1, "dead process should be restarted");
        assert!(!base.join("sock").exists(), "no socket from /bin/false");

        supervisor.stop().await;
        let restarts_at_stop = supervisor.restarts();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            supervisor.restarts(),
            restarts_at_stop,
            "no restarts after stop"
        );

        // A long-running child must be killed by stop(), not leaked.
        let base2 = std::env::temp_dir().join(format!("odorobo-sup2-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&base2).unwrap();
        let supervisor = VirtioFsSupervisor::start(VirtiofsdSpec {
            program: "/bin/sleep".into(),
            socket: base2.join("sock"),
            shared_dir: base2.clone(),
            log: base2.join("log"),
        })
        .expect("supervisor starts");
        tokio::time::sleep(Duration::from_millis(300)).await;
        supervisor.stop().await;
        // /bin/sleep should be gone: no process owns "sleep 100" anymore.
        assert!(!check_sleeping_children(), "supervised child leaked after stop");

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&base2).ok();
    }

    fn check_sleeping_children() -> bool {
        // look for a `sleep 100` process (our second supervisor's child)
        let output = std::process::Command::new("pgrep")
            .args(["-f", "sleep 100"])
            .output()
            .expect("pgrep");
        output.status.success()
    }
}
