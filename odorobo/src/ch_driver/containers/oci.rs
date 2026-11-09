//! Image reference handling, platform selection, and digest/size preflight
//! before an image is imported into the composefs repository.

use std::time::Duration;

use sha2::Digest;
use stable_eyre::{
    Result,
    eyre::{Context, bail, eyre},
};
use tokio::process::Command;

use super::process::{Tool, set_child_file_size_limit};
use super::{RemovePathOnDrop, to_hex};

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
                "image reference {image_ref:?} uses a host-local transport; set rootfs_allow_local_transports=true (or ODOROBO_ROOTFS_ALLOW_LOCAL_TRANSPORTS=true) to permit it"
            );
        }
        Ok(image_ref.to_owned())
    } else if image_ref.contains("://") {
        bail!("unsupported image transport in {image_ref:?}");
    } else {
        Ok(format!("docker://{image_ref}"))
    }
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

pub(super) struct PullPreflight {
    pub copy_ref: String,
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

pub(super) fn normalized_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    }
}

/// Resolve the runnable manifest for this node's architecture and enforce the
/// compressed-size cap before anything is pulled. The returned reference is
/// digest-pinned, so the import fetches exactly the manifest accepted here.
pub(super) async fn preflight_compressed_size(
    image_ref: &str,
    normalized: &str,
    max_bytes: u64,
) -> Result<PullPreflight> {
    let raw = inspect_raw_manifest(normalized).await?;
    let raw_len = raw.len() as u64;
    let value: serde_json::Value =
        serde_json::from_slice(&raw).wrap_err("parse raw OCI manifest during size preflight")?;
    let (manifest, manifest_bytes, copy_ref) = if value["layers"].as_array().is_some() {
        let digest = format!("sha256:{}", to_hex(&sha2::Sha256::digest(&raw)));
        let copy_ref = if normalized.starts_with("docker://") {
            docker_digest_ref(normalized, &digest)?
        } else {
            normalized.to_owned()
        };
        (value, raw_len, copy_ref)
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
        compressed_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
                err.to_string().contains("rootfs_allow_local_transports"),
                "{local}: {err}"
            );
            assert_eq!(normalize_image_ref(local, true).unwrap(), local);
        }
        let _err = normalize_image_ref("https://example/img", true).unwrap_err();
    }
}
