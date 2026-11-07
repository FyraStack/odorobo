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

use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use composefs::{
    fsverity::{Algorithm, FsVerityHashValue, Sha256HashValue},
    mount::{FsHandle, erofs_mount, mount_at},
    mountcompat::{overlayfs_set_fd, prepare_mount},
    repository::{Repository, RepositoryConfig},
};
use composefs_oci::{OciDigest, PullOptions, open_config, pull};
use rustix::fs::CWD;
use rustix::mount::{FsMountFlags, MountAttrFlags, fsconfig_create, fsconfig_set_string, fsmount};
use stable_eyre::{
    Result,
    eyre::{Context, eyre},
};
use tracing::info;

use super::limits::local_transports_allowed;
use super::oci::{normalize_image_ref, preflight_compressed_size};

/// Node-local store of fs-verity objects shared by every OCI image.
pub(super) const REPOSITORY_PATH: &str = "/var/lib/odorobo/composefs";

static REPOSITORY: OnceLock<std::result::Result<Arc<Repository<Sha256HashValue>>, String>> =
    OnceLock::new();

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
}

impl ImportedImage {
    /// Consume the import result into its manifest digest, image object, and
    /// raw OCI config document.
    pub(super) fn into_parts(self) -> (String, Sha256HashValue, Vec<u8>) {
        (self.manifest_digest, self.image, self.config)
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
    let normalized = normalize_image_ref(image_ref, local_transports_allowed())?;
    let preflight = preflight_compressed_size(image_ref, &normalized, max_compressed_bytes).await?;
    let repository = repository()?;
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
