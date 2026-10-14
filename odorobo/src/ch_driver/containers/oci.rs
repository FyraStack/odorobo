//! Skopeo pulls, digest preflight, the node-local blob CAS, and OCI manifest
//! selection and integrity checks.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::Digest;
use stable_eyre::{
    Result,
    eyre::{Context, bail, eyre},
};
use tokio::process::Command;
use tracing::{debug, info};

use super::limits;
use super::process::{Tool, set_child_file_size_limit};
use super::{OCI_BLOBS_ROOT, OCI_CACHE_ROOT, RemovePathOnDrop, remove_path, to_hex};

pub(super) const SKOPEO_TIMEOUT: Duration = Duration::from_secs(600);

/// Filesystem-safe slug for an image reference (cache dir / cache key).
/// Always a single, non-traversing path component.
pub(super) fn sanitize_image_ref(image_ref: &str) -> String {
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

/// Accepts bare docker refs and `docker://`. Host-local transports read
/// node paths or images as root, so they are honoured only when `allow_local`.
pub(super) fn normalize_image_ref(image_ref: &str, allow_local: bool) -> Result<String> {
    const LOCAL_TRANSPORTS: [&str; 6] = [
        "oci:",
        "oci-archive:",
        "dir:",
        "containers-storage:",
        "docker-archive:",
        "docker-daemon:",
    ];
    if image_ref.starts_with("docker://") {
        Ok(image_ref.to_owned())
    } else if LOCAL_TRANSPORTS.iter().any(|t| image_ref.starts_with(t)) {
        if !allow_local {
            bail!(
                "image reference {image_ref:?} uses a host-local transport; set ODOROBO_ROOTFS_ALLOW_LOCAL_TRANSPORTS=1 to permit it"
            );
        }
        Ok(image_ref.to_owned())
    } else if image_ref.contains("://") {
        bail!("unsupported image transport in {image_ref:?}");
    } else {
        Ok(format!("docker://{image_ref}"))
    }
}

#[derive(Clone, Copy, Debug)]
#[expect(
    clippy::struct_field_names,
    reason = "each field is a maximum; the prefix keeps call sites unambiguous"
)]
pub(super) struct OciUnpackLimits {
    pub max_compressed_bytes: u64,
    pub max_layer_bytes: u64,
    pub max_image_bytes: u64,
    pub max_image_entries: u64,
}

impl OciUnpackLimits {
    pub(super) fn from_env() -> Result<Self> {
        Ok(Self {
            max_compressed_bytes: limits::MAX_COMPRESSED_BYTES.read_env()?,
            max_layer_bytes: limits::MAX_LAYER_BYTES.read_env()?,
            max_image_bytes: limits::MAX_IMAGE_BYTES.read_env()?,
            max_image_entries: limits::MAX_IMAGE_ENTRIES.read_env()?,
        })
    }
}

pub(super) fn digest_blob_path(layout: &Path, digest: &str) -> Result<PathBuf> {
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
pub(super) fn globalize_layout_blobs(layout: &Path) -> Result<()> {
    globalize_layout_blobs_at(layout, Path::new(OCI_BLOBS_ROOT))
}

pub(super) fn globalize_layout_blobs_at(layout: &Path, cas_root: &Path) -> Result<()> {
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
            // Link on the same filesystem (outside blobs/, which must hold only
            // digests) and rename over the blob so a failure never leaves the
            // cached layout without it.
            // The staged name is unique: concurrent cache-hit pulls of one
            // ref may relink the same blob at once.
            let staged = layout.join(format!(".{filename}.{}.cas-link", ulid::Ulid::generate()));
            std::fs::hard_link(&cas, &staged)?;
            if let Err(err) = std::fs::rename(&staged, &blob) {
                remove_path(&staged);
                let relinked = std::fs::metadata(&blob)
                    .is_ok_and(|meta| meta.ino() == std::fs::metadata(&cas).map_or(0, |c| c.ino()));
                if !relinked {
                    return Err(err.into());
                }
            }
        }
    }
    Ok(())
}

pub(super) fn oci_layout_blob_bytes(layout: &Path) -> Result<u64> {
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

pub(super) async fn inspect_raw_manifest(image_ref: &str) -> Result<Vec<u8>> {
    const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
    let path =
        std::env::temp_dir().join(format!(".odorobo-manifest-{}.json", ulid::Ulid::generate()));
    let _cleanup = RemovePathOnDrop::new(path.clone());
    let file = std::fs::File::create(&path)?;
    let mut command = Command::new("skopeo");
    command
        .args(["inspect", "--raw", image_ref])
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::null());
    set_child_file_size_limit(&mut command, MAX_MANIFEST_BYTES)?;
    let status = Tool::bounded(Duration::from_secs(60))
        .status(command)
        .await
        .wrap_err("skopeo manifest preflight")?;
    if !status.success() {
        bail!("skopeo inspect --raw failed for {image_ref} ({status})");
    }
    tokio::fs::read(&path).await.map_err(Into::into)
}

pub(super) fn docker_digest_ref(image_ref: &str, digest: &str) -> Result<String> {
    let name = image_ref
        .strip_prefix("docker://")
        .ok_or_else(|| eyre!("cannot resolve Docker digest reference {image_ref:?}"))?;
    let repository = if let Some((repository, _tag_or_digest)) = name.split_once('@') {
        repository
    } else {
        // A tag separator is a ':' in the final path component (not a port).
        name.rsplit_once(':')
            .filter(|(_, tag)| !tag.contains('/'))
            .map_or(name, |(repository, _)| repository)
    };
    if repository.is_empty() {
        bail!("empty Docker repository in {image_ref:?}");
    }
    Ok(format!("docker://{repository}@{digest}"))
}

pub(super) fn preflight_local_oci_layout(normalized: &str) -> Result<(String, u64)> {
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

pub(super) struct PullPreflight {
    pub copy_ref: String,
    pub manifest_digest: String,
    pub compressed_bytes: u64,
}

pub(super) fn compressed_manifest_size(
    manifest: &serde_json::Value,
    manifest_bytes: u64,
) -> Result<u64> {
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

pub(super) async fn preflight_compressed_size(
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
pub(super) async fn pull_oci(image_ref: &str, max_compressed_bytes: u64) -> Result<PathBuf> {
    let image_ref = image_ref.to_owned();
    // Cancellation of an actor must not concurrently remove skopeo's staging
    // tree. The pull owns its child and cleanup until termination is reaped.
    tokio::spawn(async move { pull_oci_inner(&image_ref, max_compressed_bytes).await })
        .await
        .map_err(|err| eyre!("OCI pull task failed: {err}"))?
}

#[expect(
    clippy::too_many_lines,
    reason = "sequential setup with interleaved cleanup; splitting would obscure ownership"
)]
pub(super) async fn pull_oci_inner(image_ref: &str, max_compressed_bytes: u64) -> Result<PathBuf> {
    let normalized = normalize_image_ref(image_ref, limits::local_transports_allowed())?;
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

#[derive(Clone, Debug)]
pub(super) struct OciLayerDescriptor {
    pub digest: String,
    pub diff_id: String,
    pub media_type: String,
    pub size: Option<u64>,
}

pub(super) fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .wrap_err_with(|| format!("open {} for digest verification", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .wrap_err_with(|| format!("hash {} for digest verification", path.display()))?;
    Ok(format!("sha256:{}", to_hex(&hasher.finalize())))
}

pub(super) fn normalized_arch(arch: &str) -> &str {
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
pub(super) fn read_oci_manifest(layout: &Path) -> Result<(String, Vec<OciLayerDescriptor>)> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ch_driver::containers::layer_store::materialize_layer_at;
    use crate::ch_driver::containers::test_support::{
        TempDir, fixture_oci_layout, oci_ref, require_tools,
    };

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
        for traversal in ["..", ".", ""] {
            assert_eq!(sanitize_image_ref(traversal), "_");
        }
    }

    #[test]
    fn docker_digest_reference_replaces_tag_without_losing_registry_port() {
        assert_eq!(
            docker_digest_ref(
                "docker://registry.example:5443/org/image:stable",
                "sha256:abc"
            )
            .unwrap(),
            "docker://registry.example:5443/org/image@sha256:abc"
        );
        assert_eq!(
            docker_digest_ref("docker://registry.example/org/image", "sha256:def").unwrap(),
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
        let base = TempDir::new("blob-cas");
        let cas = base.join("cas");
        let payload = format!("blob-{}", ulid::Ulid::generate()).into_bytes();
        let digest = to_hex(&sha2::Sha256::digest(&payload));
        let mut layouts = Vec::new();
        for name in ["ref-a", "ref-b"] {
            let layout = base.join(name);
            let path = layout.join("blobs/sha256");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join(&digest), &payload).unwrap();
            globalize_layout_blobs_at(&layout, &cas).unwrap();
            layouts.push(layout);
        }
        let canonical = std::fs::metadata(cas.join(&digest)).unwrap();
        for layout in layouts {
            let copy = std::fs::metadata(layout.join("blobs/sha256").join(&digest)).unwrap();
            assert_eq!(
                copy.ino(),
                canonical.ino(),
                "reference layouts must share the CAS inode"
            );
            assert!(copy.permissions().readonly());
            assert!(
                !layout.join(format!(".{digest}.cas-link")).exists(),
                "atomic blob replacement must not leave its staging link"
            );
        }
        // Corrupt reference blob data is rejected rather than added to cache.
        let bad = base.join("bad");
        let bad_blob_dir = bad.join("blobs/sha256");
        std::fs::create_dir_all(&bad_blob_dir).unwrap();
        std::fs::write(bad_blob_dir.join(&digest), b"tampered").unwrap();
        assert!(globalize_layout_blobs_at(&bad, &cas).is_err());
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
        for invalid in [
            "sha256:../../etc/shadow",
            "sha256:deadbeef",
            "md5:aaaa",
            "garbage",
        ] {
            let _err = digest_blob_path(layout, invalid).unwrap_err();
        }
    }

    #[test]
    fn host_local_image_transports_require_node_opt_in() {
        for (image_ref, normalized) in [
            ("alpine:3", "docker://alpine:3"),
            (
                "localhost:5000/img@sha256:abc",
                "docker://localhost:5000/img@sha256:abc",
            ),
            (
                "docker://registry.example/img",
                "docker://registry.example/img",
            ),
        ] {
            assert_eq!(normalize_image_ref(image_ref, false).unwrap(), normalized);
        }
        for local in [
            "oci:/var/lib/secret:latest",
            "oci-archive:/root/img.tar",
            "dir:/etc",
            "containers-storage:private",
            "docker-archive:/root/img.tar",
            "docker-daemon:private:latest",
        ] {
            let err = normalize_image_ref(local, false).unwrap_err();
            assert!(
                err.to_string()
                    .contains("ODOROBO_ROOTFS_ALLOW_LOCAL_TRANSPORTS"),
                "{local}: {err}"
            );
            assert_eq!(normalize_image_ref(local, true).unwrap(), local);
        }
        let _err = normalize_image_ref("https://example/img", true).unwrap_err();
    }

    #[tokio::test]
    async fn compressed_pull_limit_rejects_and_removes_partial_layout() {
        if !require_tools(&["skopeo"]) {
            return;
        }
        let base = TempDir::new("pull-limit");
        let (layout, _) = fixture_oci_layout(&base);
        let image_ref = oci_ref(&layout);
        std::fs::create_dir_all(OCI_CACHE_ROOT).unwrap();
        let alias = PathBuf::from(OCI_CACHE_ROOT).join(format!(
            "{}-{}",
            sanitize_image_ref(&image_ref),
            to_hex(&sha2::Sha256::digest(image_ref.as_bytes()))
        ));
        assert!(
            pull_oci(&image_ref, 1).await.is_err(),
            "large image must fail the compressed-byte cap"
        );
        assert!(
            !alias.join("index.json").exists(),
            "an over-limit layout must not be published"
        );
        let leftovers = std::fs::read_dir(OCI_CACHE_ROOT)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".pull-"))
            .count();
        assert_eq!(leftovers, 0, "partial pull directories must be removed");
        if alias.exists() {
            std::fs::remove_dir_all(alias).unwrap();
        }
    }

    #[tokio::test]
    async fn verifies_manifest_platform_config_and_layer_descriptors() {
        let base = TempDir::new("oci-index");
        let (layout, layer) = fixture_oci_layout(&base);
        let (_, descriptors) = read_oci_manifest(&layout).unwrap();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].digest, layer.digest);
        let mut bad_diff_id = layer.clone();
        bad_diff_id.diff_id = format!("sha256:{}", "b".repeat(64));
        let _err = materialize_layer_at(&layout, &bad_diff_id, &base.join("cache-diffid"))
            .await
            .unwrap_err();
        let blob = digest_blob_path(&layout, &layer.digest).unwrap();
        assert_eq!(hash_file(&blob).unwrap(), layer.digest);
        // Any manifest, config, or layer content change invalidates its descriptor.
        let mut bytes = std::fs::read(&blob).unwrap();
        bytes[0] ^= 1;
        std::fs::write(&blob, bytes).unwrap();
        let _err = materialize_layer_at(&layout, &layer, &base.join("cache"))
            .await
            .unwrap_err();
    }
}
