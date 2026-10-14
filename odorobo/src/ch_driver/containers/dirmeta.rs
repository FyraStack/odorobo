//! Directory metadata composition across stacked layers (OCI directory
//! entries override lower-layer metadata, which `OverlayFS` does not).

use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use stable_eyre::{
    Result,
    eyre::{Context, bail, eyre},
};

use super::layer_store::ensure_fsverity;
use super::mount::unmount_path;
use super::process::Tool;
use super::unpack::{DirectoryReset, apply_xattrs};

pub(super) fn copy_directory_attributes(source: &Path, destination: &Path) -> Result<()> {
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
    let mut attributes = Vec::new();
    for name in xattr::list(source).wrap_err("list directory xattrs")? {
        let name = name.into_encoded_bytes();
        if name.starts_with(b"trusted.overlay.") || name.starts_with(b"user.overlay.") {
            continue;
        }
        let os_name = std::ffi::OsStr::from_bytes(&name);
        // An attribute removed between list and get is simply absent.
        if let Some(value) = xattr::get(source, os_name).wrap_err("read directory xattr")? {
            attributes.push((name, value));
        }
    }
    apply_xattrs(destination, &attributes)
}

pub(super) fn directory_metadata_source(
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

pub(super) static DIRECTORY_OPERATIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) async fn compose_directory_metadata(
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

pub(super) async fn compose_directory_metadata_inner(
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
    Tool::ORDERED
        .run(
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
    Tool::BULK
        .run(
            "mkcomposefs",
            [tree.display().to_string(), image.display().to_string()],
            &[],
        )
        .await?;
    ensure_fsverity(&image).await?;
    let digest = Tool::BULK
        .run("composefs-info", ["measure-file".into()], &[&image])
        .await?;
    let target = runtime.join("rootfs-directory-metadata");
    tokio::fs::create_dir_all(&target).await?;
    Tool::ORDERED
        .run(
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
    Tool::ORDERED
        .run(
            "mount",
            ["-o".into(), "remount,bind,ro,nosuid,nodev".into()],
            &[&target],
        )
        .await?;
    Tool::ORDERED
        .run("mount", ["--make-rprivate".into()], &[&target])
        .await?;
    tokio::fs::remove_dir_all(tree).await?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ch_driver::containers::test_support::{TempDir, mount};

    fn reset(descendants_only: bool) -> [Vec<DirectoryReset>; 3] {
        [
            vec![],
            vec![DirectoryReset {
                path: "app".into(),
                descendants_only,
            }],
            vec![],
        ]
    }

    #[test]
    fn directory_metadata_does_not_cross_replacement_generations_or_symlinks() {
        let base = TempDir::new("generation");
        let layers = [base.join("a"), base.join("b"), base.join("c")];
        for layer in &layers {
            std::fs::create_dir_all(layer).unwrap();
        }
        std::fs::create_dir_all(layers[0].join("app/private")).unwrap();
        std::fs::create_dir_all(layers[2].join("app/private")).unwrap();
        let changes = [vec!["app/private".into()], vec![], vec!["app".into()]];
        let no_resets = [vec![], vec![], vec![]];
        let source = |path: &str, changes: &[Vec<PathBuf>], resets: &[Vec<DirectoryReset>]| {
            directory_metadata_source(Path::new(path), &layers, changes, resets).unwrap()
        };
        assert_eq!(source("app/private", &changes, &reset(false)), 2);
        std::fs::write(layers[1].join("app"), b"replacement").unwrap();
        assert_eq!(source("app/private", &changes, &no_resets), 2);
        std::fs::remove_file(layers[1].join("app")).unwrap();
        std::os::unix::fs::symlink("/etc", layers[1].join("app")).unwrap();
        assert_eq!(source("app/private", &changes, &no_resets), 2);
        std::fs::remove_file(layers[1].join("app")).unwrap();
        std::fs::create_dir_all(layers[1].join("app/private")).unwrap();
        assert_eq!(source("app/private", &changes, &reset(false)), 1);
        let opaque = reset(true);
        assert_eq!(
            source("app", &[vec!["app".into()], vec![], vec![]], &opaque),
            0
        );
        assert_eq!(source("app/private", &changes, &opaque), 1);
    }

    #[tokio::test]
    #[ignore = "requires composefs/OverlayFS mounts and fs-verity"]
    async fn synthetic_layer_parents_preserve_inherited_directory_metadata() {
        let vmid = ulid::Ulid::generate().to_string();
        let runtime = crate::ch_driver::VMInstance::runtime_dir_for(&vmid);
        let base = runtime.join("base");
        let top = runtime.join("top");
        let merged = runtime.join("merged");
        for path in [&base, &top, &merged] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::create_dir_all(base.join("app")).unwrap();
        std::fs::create_dir_all(top.join("app")).unwrap();
        std::fs::write(base.join("app/old"), b"old").unwrap();
        std::fs::write(top.join("app/new"), b"new").unwrap();
        for path in [&base, &base.join("app")] {
            let file = std::fs::File::open(path).unwrap();
            // SAFETY: live fd; disposable test metadata.
            assert_eq!(unsafe { libc::fchown(file.as_raw_fd(), 1000, 1000) }, 0);
            file.set_permissions(std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let correction = compose_directory_metadata(
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
        mount("overlay", Some(options), &merged).await;
        for path in [&merged, &merged.join("app")] {
            let metadata = std::fs::metadata(path).unwrap();
            assert_eq!(
                (metadata.uid(), metadata.gid(), metadata.mode() & 0o7777),
                (1000, 1000, 0o700)
            );
        }
        assert_eq!(std::fs::read(merged.join("app/new")).unwrap(), b"new");
        assert_eq!(std::fs::read(merged.join("app/old")).unwrap(), b"old");
        unmount_path(&merged).await.unwrap();
        unmount_path(&correction).await.unwrap();
        std::fs::remove_dir_all(&runtime).unwrap();
        std::fs::remove_dir_all(Path::new("/var/lib/odorobo/directory-metadata").join(vmid))
            .unwrap();
    }
}
