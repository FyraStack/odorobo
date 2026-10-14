//! Digest-addressed composefs/fs-verity layer cache.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use stable_eyre::{
    Result,
    eyre::{Context, bail, eyre},
};

use super::RemovePathOnDrop;
use super::oci::{OciLayerDescriptor, digest_blob_path, hash_file};
use super::process::Tool;
use super::unpack::{LayerStats, compression_of_media_type, unpack_layer};

// Cache format v6 records explicit directory changes for composition, and
// preserves root directory metadata and restores ACLs
// after extraction to prevent order-dependent child inheritance.
// Older entries lost this metadata and must not be reused. The cache stores
// composefs storage-usage statistics alongside each immutable layer. Versioned
// paths avoid replacing older entries while they may still be mounted.
pub(super) const LAYERS_ROOT: &str = "/var/lib/odorobo/layers/sha256/v8";

pub(super) async fn ensure_fsverity(path: &Path) -> Result<()> {
    match Tool::BULK
        .run("fsverity", ["enable".to_owned()], &[path])
        .await
    {
        Ok(_) => Ok(()),
        Err(enable_error) => {
            // mkcomposefs may enable verity as part of digest-store creation;
            // an already-enabled inode returns EEXIST, accepted only if it can
            // be measured successfully.
            match Tool::BULK
                .run("fsverity", ["digest".to_owned()], &[path])
                .await
            {
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
pub(super) async fn materialize_layer(
    layout: &Path,
    desc: &OciLayerDescriptor,
    max_bytes: u64,
    max_entries: u64,
) -> Result<(PathBuf, LayerStats)> {
    materialize_layer_at_limited(layout, desc, Path::new(LAYERS_ROOT), max_bytes, max_entries).await
}

#[cfg(test)]
pub(super) async fn materialize_layer_at(
    layout: &Path,
    desc: &OciLayerDescriptor,
    cache_root: &Path,
) -> Result<PathBuf> {
    let limits = super::oci::OciUnpackLimits::from_env()?;
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

pub(super) async fn verify_cached_layer(
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
    let measured = Tool::BULK
        .run(
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

pub(super) async fn materialize_layer_at_limited(
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

#[expect(
    clippy::too_many_lines,
    reason = "sequential setup with interleaved cleanup; splitting would obscure ownership"
)]
pub(super) async fn materialize_layer_inner(
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
    std::fs::DirBuilder::new().mode(0o700).create(&tmp)?;
    std::fs::create_dir_all(&tree)?;
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
        Tool::BULK
            .run(
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
        let verity_digest = Tool::BULK
            .run(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ch_driver::containers::test_support::{
        TempDir, fixture_oci_layout, require_tools, umount,
    };

    fn has_fsverity(path: &Path) -> bool {
        std::process::Command::new("fsverity")
            .arg("digest")
            .arg(path)
            .status()
            .unwrap()
            .success()
    }

    #[tokio::test]
    async fn materializes_and_reuses_one_immutable_fsverity_layer_cache_entry() {
        if !require_tools(&["mkcomposefs", "fsverity"]) {
            return;
        }
        let base = TempDir::under(Path::new("/var/lib/odorobo/test-layers"), "cache");
        let (layout, layer) = fixture_oci_layout(&base);
        let second_layout = base.join("other-reference");
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
        assert!(has_fsverity(&first.join("layer.cfs")));
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
                has_fsverity(&object),
                "backing payload must have fs-verity enabled"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires privileged mounts and host composefs/fs-verity kernel support"]
    async fn composefs_layer_mount_verifies_image_and_backing_files() {
        let base = TempDir::under(Path::new("/var/lib/odorobo/composefs-mount-test"), "mount");
        let (layout, layer) = fixture_oci_layout(&base);
        let cache = materialize_layer_at(&layout, &layer, &base.join("cache"))
            .await
            .unwrap();
        let target = base.join("mnt");
        std::fs::create_dir_all(&target).unwrap();
        let digest = std::fs::read_to_string(cache.join("layer-digest")).unwrap();
        let options = format!(
            "basedir={},digest={},verity,ro",
            cache.join("store").display(),
            digest.trim()
        );
        Tool::ORDERED
            .run(
                "mount.composefs",
                ["-o".to_owned(), options],
                &[&cache.join("layer.cfs"), &target],
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(target.join("etc/x")).unwrap(),
            vec![b'x'; 16 * 1024]
        );
        umount(&target).await;
    }
}
