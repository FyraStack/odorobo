//! Node-local composefs repository backing every OCI rootfs.
//!
//! An image is imported once: `composefs-oci` unpacks its layers into
//! content-addressed fs-verity objects under [`REPOSITORY_PATH`] and links the
//! image config to a single EROFS metadata object. Every VM then mounts that
//! object through the kernel overlay with its own upper directory, so nothing
//! is copied per VM and layers shared by several images exist once.
//!
//! Security notes: the mount is digest-pinned, so the kernel verifies the
//! image object against its fs-verity id at mount time. The repository is only
//! as trustworthy as its filesystem, which must support fs-verity (ext4,
//! btrfs, erofs); a repository without verity metadata mounts unverified.

use std::collections::BTreeSet;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};

use composefs::{
    fsverity::{Algorithm, FsVerityHashValue, Sha256HashValue},
    mount::{FsHandle, erofs_mount, mount_at},
    mountcompat::{overlayfs_set_fd, prepare_mount},
    repository::{GcResult, Repository, RepositoryConfig},
};
use composefs_oci::oci_image::manifest_identifier;
use composefs_oci::{OciDigest, PullOptions, open_config, pull};
use rustix::fs::CWD;
use rustix::mount::{FsMountFlags, MountAttrFlags, fsconfig_create, fsconfig_set_string, fsmount};
use stable_eyre::{
    Result,
    eyre::{Context, eyre},
};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::info;

use super::limits::policy;
use super::oci::{normalize_image_ref, preflight_compressed_size};

/// Node-local store of fs-verity objects shared by every OCI image.
pub(super) const REPOSITORY_PATH: &str = "/var/lib/odorobo/composefs";

static REPOSITORY: OnceLock<std::result::Result<Arc<Repository<Sha256HashValue>>, String>> =
    OnceLock::new();

/// Serializes repository mutations against garbage collection. An import holds
/// the guard from its first write until the imported image is mounted.
static REPOSITORY_WRITES: LazyLock<Arc<Mutex<()>>> = LazyLock::new(|| Arc::new(Mutex::new(())));

/// Shared repository handle; the first call creates the store when absent.
pub(super) fn repository() -> Result<Arc<Repository<Sha256HashValue>>> {
    match REPOSITORY.get_or_init(|| {
        open_repository()
            .map(Arc::new)
            .map_err(|error| format!("{error:#}"))
    }) {
        Ok(repository) => Ok(Arc::clone(repository)),
        Err(message) => Err(eyre!(
            "composefs repository {REPOSITORY_PATH} is unavailable: {message}"
        )),
    }
}

fn open_repository() -> Result<Repository<Sha256HashValue>> {
    std::fs::create_dir_all(REPOSITORY_PATH)
        .wrap_err_with(|| format!("creating {REPOSITORY_PATH}"))?;
    Repository::init_path(
        CWD,
        REPOSITORY_PATH,
        RepositoryConfig::new(Algorithm::for_hash::<Sha256HashValue>()),
    )
    .map(|(repository, fresh)| {
        if fresh {
            info!(path = REPOSITORY_PATH, "composefs repository created");
        }
        repository
    })
    .map_err(|error| eyre!("initializing {REPOSITORY_PATH}: {error:#}"))
}

/// An OCI image imported into the repository, ready to mount for any VM.
#[derive(Debug)]
pub(super) struct ImportedImage {
    /// OCI manifest digest identifying the immutable base.
    manifest_digest: String,
    /// fs-verity object id of the image's EROFS metadata object.
    image: Sha256HashValue,
    /// Raw OCI image config document, delivered to the guest init.
    config: Vec<u8>,
    /// Blocks repository garbage collection until this image is mounted, so a
    /// freshly imported image is never collected before its first mount.
    write_guard: OwnedMutexGuard<()>,
}

impl ImportedImage {
    /// Consume the import result into its manifest digest, image object, raw
    /// config document, and repository write guard.
    pub(super) fn into_parts(self) -> (String, Sha256HashValue, Vec<u8>, OwnedMutexGuard<()>) {
        (
            self.manifest_digest,
            self.image,
            self.config,
            self.write_guard,
        )
    }
}

/// Import `image_ref` if it is not stored yet, and return its mountable form.
pub(super) async fn import_image(
    image_ref: &str,
    max_compressed_bytes: u64,
) -> Result<ImportedImage> {
    // The importer future is not `Send` (the layer service holds a boxed reply
    // stream), so it runs on its own runtime on a blocking thread.
    let image_ref = image_ref.to_owned();
    tokio::task::spawn_blocking(move || import_image_blocking(&image_ref, max_compressed_bytes))
        .await
        .wrap_err("joining the composefs import task")?
}

fn import_image_blocking(image_ref: &str, max_compressed_bytes: u64) -> Result<ImportedImage> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .wrap_err("building the composefs import runtime")?;
    runtime.block_on(import_image_inner(image_ref, max_compressed_bytes))
}

#[expect(
    clippy::future_not_send,
    reason = "the importer runs on the dedicated single-threaded runtime"
)]
async fn import_image_inner(image_ref: &str, max_compressed_bytes: u64) -> Result<ImportedImage> {
    let normalized = normalize_image_ref(image_ref, policy().allow_local_transports)?;
    let preflight = preflight_compressed_size(image_ref, &normalized, max_compressed_bytes).await?;
    let repository = repository()?;
    // Wait out a running GC; the guard travels with the result to first mount.
    let write_guard = Arc::clone(&REPOSITORY_WRITES).lock_owned().await;
    // Pulling the digest-pinned reference from the preflight imports exactly
    // the manifest whose platform and compressed size were just accepted.
    let imported = pull(
        &repository,
        &preflight.copy_ref,
        None,
        PullOptions::default(),
    )
    .await
    .map_err(|error| {
        eyre!("importing {image_ref} into the composefs repository failed: {error:#}")
    })?;
    let opened = open_config(
        &repository,
        &imported.config_digest,
        Some(&imported.config_verity),
    )
    .map_err(|error| eyre!("reading the stored config of {image_ref} failed: {error:#}"))?;
    let image = opened.image_ref.or(opened.image_ref_v1).ok_or_else(|| {
        eyre!("the composefs import of {image_ref} produced no bootable EROFS image object")
    })?;
    let config = read_config(
        &repository,
        &imported.config_digest,
        &imported.config_verity,
    )?;
    let manifest_digest = imported.manifest_digest.to_string();
    info!(
        image = image_ref,
        %manifest_digest,
        image_object = %image.to_hex(),
        layers = opened.layer_refs.len(),
        new_objects = imported.stats.new_objects(),
        new_bytes = imported.stats.new_bytes(),
        compressed_bytes = preflight.compressed_bytes,
        "OCI image imported into the composefs repository"
    );
    Ok(ImportedImage {
        manifest_digest,
        image,
        config,
        write_guard,
    })
}

/// Mount the composed root for a VM at `mountpoint` with one overlayfs mount:
/// the image's EROFS metadata object and `config_dir` are the lower layers and
/// the repository's object store supplies file data. `overlay_upper` adds the
/// per-VM writable directories, and `read_write` only matters with them.
pub(super) fn mount_image(
    image: &Sha256HashValue,
    mountpoint: &Path,
    config_dir: &Path,
    overlay_upper: Option<(&Path, &Path)>,
    read_write: bool,
) -> Result<()> {
    let repository = repository()?;
    let (image_fd, enable_verity) = repository
        .open_digest(image)
        .map_err(|error| eyre!("opening the composed image object failed: {error:#}"))?;
    let objects = repository
        .objects_dir()
        .map_err(|error| eyre!("opening the repository object store failed: {error:#}"))?;
    let erofs = erofs_mount(image_fd)
        .map_err(|error| eyre!("mounting the image metadata failed: {error:#}"))?;
    let erofs = prepare_mount(erofs)
        .map_err(|error| eyre!("preparing the image metadata mount failed: {error:#}"))?;
    let overlayfs = FsHandle::open("overlay")
        .map_err(|error| eyre!("opening an overlayfs context failed: {error:#}"))?;
    set_overlay_option(
        &overlayfs,
        "source",
        &format!("composefs:{}", image.to_hex()),
    )?;
    set_overlay_option(&overlayfs, "metacopy", "on")?;
    set_overlay_option(&overlayfs, "redirect_dir", "on")?;
    if enable_verity {
        set_overlay_option(&overlayfs, "verity", "require")?;
    }
    // The config directory is the first lower, so the kernel prefers it over
    // the image for paths that exist in both.
    set_overlay_fd(&overlayfs, "lowerdir+", &open_overlay_dir(config_dir)?)?;
    set_overlay_fd(&overlayfs, "lowerdir+", &erofs)?;
    set_overlay_fd(&overlayfs, "datadir+", objects)?;
    if let Some((upper, work)) = overlay_upper {
        set_overlay_fd(&overlayfs, "upperdir", &open_overlay_dir(upper)?)?;
        set_overlay_fd(&overlayfs, "workdir", &open_overlay_dir(work)?)?;
    }
    fsconfig_create(overlayfs.as_fd()).map_err(|error| {
        eyre!(
            "assembling the composed root of {} failed: {error}",
            mountpoint.display()
        )
    })?;
    let attributes = if read_write {
        MountAttrFlags::empty()
    } else {
        MountAttrFlags::MOUNT_ATTR_RDONLY
    } | MountAttrFlags::MOUNT_ATTR_NOSUID
        | MountAttrFlags::MOUNT_ATTR_NODEV;
    let fd =
        fsmount(overlayfs.as_fd(), FsMountFlags::FSMOUNT_CLOEXEC, attributes).map_err(|error| {
            eyre!(
                "mounting the composed root of {} failed: {error}",
                mountpoint.display()
            )
        })?;
    mount_at(fd, CWD, mountpoint).map_err(|error| {
        eyre!(
            "attaching the composed root at {} failed: {error}",
            mountpoint.display()
        )
    })?;
    Ok(())
}

fn set_overlay_option(handle: &FsHandle, key: &str, value: &str) -> Result<()> {
    fsconfig_set_string(handle.as_fd(), key, value)
        .map_err(|error| eyre!("setting overlayfs {key}={value} failed: {error}"))
}

fn set_overlay_fd(handle: &FsHandle, key: &str, fd: impl std::os::fd::AsFd) -> Result<()> {
    overlayfs_set_fd(handle.as_fd(), key, fd.as_fd())
        .map_err(|error| eyre!("setting overlayfs {key} failed: {error}"))
}

fn open_overlay_dir(path: &Path) -> Result<OwnedFd> {
    std::fs::File::open(path)
        .map(Into::into)
        .wrap_err_with(|| format!("opening overlay directory {}", path.display()))
}

/// Repository stream name of an OCI manifest digest. Rooting it keeps the whole
/// image reachable: config, layers, EROFS metadata object, and file payloads.
fn manifest_stream_name(manifest_digest: &str) -> Result<String> {
    let digest: OciDigest = manifest_digest
        .parse()
        .wrap_err_with(|| format!("invalid OCI manifest digest {manifest_digest:?}"))?;
    Ok(manifest_identifier(&digest))
}

/// Reclaim every repository object not reachable from `roots`, from a stored
/// reference, or from a live mount. Blocking: run it off the async runtime.
fn gc_repository(
    repository: &Repository<Sha256HashValue>,
    roots: &[String],
    dry_run: bool,
) -> Result<GcResult> {
    let roots: Vec<&str> = roots.iter().map(String::as_str).collect();
    let result = if dry_run {
        repository.gc_dry_run(&roots)
    } else {
        repository.gc(&roots)
    }
    .map_err(|error| eyre!("composefs repository garbage collection failed: {error:#}"))?;
    info!(
        dry_run,
        roots = roots.len(),
        objects_removed = result.objects_removed,
        objects_bytes = result.objects_bytes,
        images_pruned = result.images_pruned,
        streams_pruned = result.streams_pruned,
        "composefs repository garbage collection finished"
    );
    Ok(result)
}

/// Reclaim repository space while protecting every image the node may still
/// use.
///
/// `gather_vm_digests` runs after the write guard is taken, so every imported
/// image either has its digest in the gathered set or still holds its own
/// guard until mount. Persistent upper state pins its base digest
/// independently of any VM actor. Everything else — unused images, streams,
/// and content-addressed objects — is removed.
pub async fn collect_repository_garbage<Gather, Gathered>(
    gather_vm_digests: Gather,
    dry_run: bool,
) -> Result<GcResult>
where
    Gather: FnOnce() -> Gathered + Send,
    Gathered: std::future::Future<Output = Result<Vec<String>>> + Send,
{
    let repository = repository()?;
    // Waits out running imports; finished imports hold the guard to their mount.
    let _writer = Arc::clone(&REPOSITORY_WRITES).lock_owned().await;
    let mut roots = BTreeSet::new();
    for digest in gather_vm_digests().await? {
        roots.insert(manifest_stream_name(&digest)?);
    }
    for digest in super::persistent::pinned_base_digests().await? {
        roots.insert(manifest_stream_name(&digest)?);
    }
    let roots: Vec<String> = roots.into_iter().collect();
    tokio::task::spawn_blocking(move || gc_repository(&repository, &roots, dry_run))
        .await
        .wrap_err("joining the repository garbage collection task")?
}

fn read_config(
    repository: &Repository<Sha256HashValue>,
    config_digest: &OciDigest,
    verity: &Sha256HashValue,
) -> Result<Vec<u8>> {
    let mut stream = repository
        .open_stream(&format!("oci-config-{config_digest}"), Some(verity), None)
        .map_err(|error| eyre!("opening the stored OCI config stream failed: {error:#}"))?;
    let mut config = Vec::new();
    stream
        .cat(repository, &mut config)
        .map_err(|error| eyre!("reading the stored OCI config stream failed: {error:#}"))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ch_driver::containers::test_support::TempDir;
    use std::path::PathBuf;

    fn test_repository(dir: &Path) -> Arc<Repository<Sha256HashValue>> {
        let (repository, _) = Repository::init_path(
            CWD,
            dir,
            RepositoryConfig::new(Algorithm::for_hash::<Sha256HashValue>()).set_insecure(),
        )
        .unwrap();
        Arc::new(repository)
    }

    fn digest(fill: char) -> String {
        format!("sha256:{}", String::from(fill).repeat(64))
    }

    /// Writes a `oci-manifest-*` splitstream holding `payload` and returns the
    /// payload object id.
    fn write_manifest_stream(
        repository: &Arc<Repository<Sha256HashValue>>,
        manifest_digest: &str,
        payload: &[u8],
    ) -> Sha256HashValue {
        let object = repository.ensure_object(payload).unwrap();
        let mut writer = repository.create_stream(0).unwrap();
        writer.write_reference(object.clone()).unwrap();
        repository
            .write_stream(
                writer,
                &manifest_stream_name(manifest_digest).unwrap(),
                None,
            )
            .unwrap();
        object
    }

    fn object_path(dir: &Path, object: &Sha256HashValue) -> PathBuf {
        dir.join("objects").join(object.to_object_pathname())
    }

    #[test]
    fn gc_keeps_rooted_images_and_removes_the_rest() {
        let dir = TempDir::new("repo-gc");
        let repository = test_repository(&dir);
        let kept = write_manifest_stream(&repository, &digest('a'), b"kept payload");
        let garbage = write_manifest_stream(&repository, &digest('b'), b"garbage payload");

        let roots = vec![manifest_stream_name(&digest('a')).unwrap()];
        let result = gc_repository(&repository, &roots, false).unwrap();

        assert!(
            repository
                .has_stream(&manifest_stream_name(&digest('a')).unwrap())
                .unwrap()
                .is_some(),
            "rooted stream must survive"
        );
        assert!(
            repository
                .has_stream(&manifest_stream_name(&digest('b')).unwrap())
                .unwrap()
                .is_none(),
            "unrooted stream must be pruned"
        );
        assert!(object_path(&dir, &kept).exists());
        assert!(!object_path(&dir, &garbage).exists());
        assert!(result.objects_removed >= 1);
    }

    #[test]
    fn gc_dry_run_reports_without_removing() {
        let dir = TempDir::new("repo-gc-dry");
        let repository = test_repository(&dir);
        let payload = write_manifest_stream(&repository, &digest('b'), b"dry-run payload");

        let result = gc_repository(&repository, &[], true).unwrap();

        assert!(
            repository
                .has_stream(&manifest_stream_name(&digest('b')).unwrap())
                .unwrap()
                .is_some()
        );
        assert!(object_path(&dir, &payload).exists());
        assert!(result.objects_removed >= 1);
    }

    #[test]
    fn manifest_stream_names_stay_in_sync_with_composefs_oci() {
        let manifest_digest = digest('c');
        let parsed: OciDigest = manifest_digest.parse().unwrap();
        assert_eq!(
            manifest_stream_name(&manifest_digest).unwrap(),
            manifest_identifier(&parsed)
        );
        let error =
            manifest_stream_name("sha256:nothex").expect_err("malformed digest must be rejected");
        assert!(error.to_string().contains("invalid OCI manifest digest"));
        let error =
            manifest_stream_name("not-a-digest").expect_err("non-hex digest must be rejected");
        assert!(error.to_string().contains("invalid OCI manifest digest"));
    }
}
