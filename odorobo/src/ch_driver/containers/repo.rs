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

use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use composefs::{
    fsverity::{Algorithm, FsVerityHashValue, Sha256HashValue},
    mount::{MountOptions, mount_at},
    repository::{Repository, RepositoryConfig},
};
use composefs_oci::{OciDigest, PullOptions, open_config, pull};
use rustix::fs::CWD;
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

/// Mount the image's EROFS object at `mountpoint`, optionally through a
/// per-VM overlay upper/work pair (which makes the mount writable).
pub(super) fn mount_image(
    image: &Sha256HashValue,
    mountpoint: &Path,
    overlay_upper: Option<(&Path, &Path)>,
) -> Result<()> {
    let repository = repository()?;
    let mut options = MountOptions::default();
    if let Some((upper, work)) = overlay_upper {
        options.set_overlay(open_overlay_dir(upper)?, open_overlay_dir(work)?);
        options.set_read_write(true);
    }
    let fd = repository.mount_digest(image, &options).map_err(|error| {
        eyre!(
            "assembling the composefs root of {} failed: {error:#}",
            mountpoint.display()
        )
    })?;
    mount_at(fd, CWD, mountpoint).map_err(|error| {
        eyre!(
            "attaching the composefs root at {} failed: {error:#}",
            mountpoint.display()
        )
    })?;
    Ok(())
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
