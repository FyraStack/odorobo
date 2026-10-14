//! Persistent writable upper state, backed by a local directory or Ceph RBD.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use stable_eyre::{
    Result,
    eyre::{Context, bail, eyre},
};
use tokio::process::Command;

use super::limits::{MIB, RBD_SIZE};
use super::mount::{is_mountpoint, mount_ephemeral_state, unmount_path};
use super::process::Tool;
use super::{PERSISTENT_ROOTFS_ROOT, validate_vmid};
use crate::manifest::RootfsMode;

/// Runtime-dir subdirectory holding the VM-private, size-bounded tmpfs that
/// backs an ephemeral root's `OverlayFS` upper and work directories.
pub(super) const EPHEMERAL_STATE_DIR: &str = "rootfs-state";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PersistentBackend {
    /// Plain directory under `PERSISTENT_ROOTFS_ROOT` on the host filesystem.
    /// No per-VM quota is enforced: a guest can fill that host filesystem
    /// (typically /var). Use `Rbd` where guest writes must be bounded.
    Local,
    /// Per-VM Ceph RBD image whose fixed size bounds guest writes.
    Rbd,
}

pub(super) fn persistent_backend() -> Result<PersistentBackend> {
    match std::env::var("ODOROBO_ROOTFS_BACKEND")
        .unwrap_or_else(|_| "local".to_owned())
        .as_str()
    {
        "local" => Ok(PersistentBackend::Local),
        "rbd" => Ok(PersistentBackend::Rbd),
        other => bail!("ODOROBO_ROOTFS_BACKEND must be 'local' or 'rbd', not {other:?}"),
    }
}

pub(super) fn rbd_prefix_args() -> Vec<String> {
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

pub(super) fn rbd_pool() -> String {
    std::env::var("ODOROBO_ROOTFS_RBD_POOL")
        .or_else(|_| std::env::var("CEPH_POOL"))
        .unwrap_or_else(|_| "odorobo-blockpool".to_owned())
}

pub(super) async fn rbd_lookup_device(image: &str) -> Result<Option<PathBuf>> {
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

pub(super) fn match_rbd_head_device(
    image: &str,
    devices: &serde_json::Value,
) -> Result<Option<PathBuf>> {
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

/// `rbd create --size` argument, canonicalised to MiB.
pub(super) fn rbd_size() -> Result<String> {
    Ok(format!("{}M", RBD_SIZE.read_env()? / MIB))
}

pub(super) fn rbd_identity(vmid: &str) -> Result<String> {
    validate_vmid(vmid)?;
    Ok(format!("{}/odorobo-rootfs-{vmid}", rbd_pool()))
}

pub(super) async fn acquire_rootfs_lock(vmid: &str) -> Result<Arc<std::fs::File>> {
    validate_vmid(vmid)?;
    let dir = PathBuf::from("/run/odorobo/rootfs-locks");
    tokio::fs::create_dir_all(&dir).await?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
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

/// Keep command ownership outside the caller's cancellable future. Cleanup
/// takes the same operation lock ([`Tool::ORDERED`]), so it cannot inspect or
/// unmap before a cancelled acquisition has completed (or timed out, been
/// killed and reaped).
pub(super) async fn rbd_output(args: &[String]) -> Result<std::process::Output> {
    let mut command = Command::new("rbd");
    command.args(rbd_prefix_args()).args(args);
    Tool::ORDERED.output(command).await
}

pub(super) async fn rbd_image_exists(image: &str) -> Result<bool> {
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

pub static PERSISTENT_OPERATIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub static RBD_RESERVATIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(Default::default);

// None = command started but device not yet established; absence of a key =
// never acquired (including explicit preflight rejection).
pub(super) static RBD_STAGING_IMAGES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
> = std::sync::LazyLock::new(Default::default);

pub(super) async fn cleanup_staging_images(image: &str) -> Result<()> {
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

pub(super) static PERSISTENT_RBD_OWNERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, Option<PathBuf>>>,
> = std::sync::LazyLock::new(Default::default);

pub(super) async fn mount_persistent_state(vmid: &str) -> Result<(PathBuf, Option<String>)> {
    let guard = PERSISTENT_OPERATIONS.lock().await;
    let vmid = vmid.to_owned();
    tokio::spawn(async move {
        let _guard = guard;
        mount_persistent_state_inner(&vmid).await
    })
    .await
    .map_err(|err| eyre!("persistent attachment task failed: {err}"))?
}

#[expect(
    clippy::too_many_lines,
    reason = "sequential setup with interleaved cleanup; splitting would obscure ownership"
)]
pub(super) async fn mount_persistent_state_inner(vmid: &str) -> Result<(PathBuf, Option<String>)> {
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
            Tool::BULK
                .run("mkfs.ext4", ["-F".into()], &[&device])
                .await?;
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
            let mut check = Command::new("e2fsck");
            check.arg("-p").arg(&device);
            let check = Tool::BULK.output(check).await?;
            if !matches!(check.status.code(), Some(0 | 1)) {
                bail!(
                    "RBD filesystem check failed ({}): {}",
                    check.status,
                    String::from_utf8_lossy(&check.stderr).trim()
                );
            }
        }
        Tool::ORDERED
            .run(
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

pub(super) async fn is_device_mounted(device: &Path) -> Result<bool> {
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

pub(super) struct UpperState {
    pub upper: Option<PathBuf>,
    pub work: Option<PathBuf>,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "inspected by persistence tests")
    )]
    pub state_mount: Option<PathBuf>,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "inspected by persistence tests")
    )]
    pub rbd_image: Option<String>,
}

pub(super) async fn prepare_upper(
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
        // Never place guest-writable state directly in the runtime dir: it
        // lives on host tmpfs (/run) and would let a guest exhaust host RAM.
        // The caller records this mount in the owner before we get here.
        let path = runtime_dir.join(EPHEMERAL_STATE_DIR);
        mount_ephemeral_state(&path).await?;
        path
    };
    let result: Result<(PathBuf, PathBuf)> = async {
        let upper = state_dir.join("rootfs.upper");
        let work = state_dir.join("rootfs.work");
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
            if mode == RootfsMode::Ephemeral
                && let Err(cleanup) = unmount_path(&state_dir).await
            {
                return Err(err.wrap_err(format!(
                    "ephemeral rootfs preparation failed; rollback failed: {cleanup}"
                )));
            }
            if let Some(image) = &rbd_image
                && let Err(cleanup) =
                    release_persistent_backend(Some(&state_dir), Some(image)).await
            {
                return Err(err.wrap_err(format!(
                    "persistent rootfs preparation failed; rollback failed: {cleanup}"
                )));
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

pub(super) async fn release_persistent_backend(
    state_mount: Option<&Path>,
    rbd_image: Option<&str>,
) -> Result<()> {
    if state_mount.is_none() && rbd_image.is_none() {
        // Nothing to release: don't queue ephemeral/read-only teardown behind
        // another VM's long persistent attach (mkfs/e2fsck).
        return Ok(());
    }
    let _guard = PERSISTENT_OPERATIONS.lock().await;
    release_persistent_backend_inner(state_mount, rbd_image).await
}

pub(super) async fn release_persistent_backend_inner(
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

/// Remove persistent upper state only from the explicit `DeleteVM` path.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_rbd_lookup_matches_only_default_namespace_head() {
        let mappings = serde_json::json!([
            {"pool":"pool", "namespace":"foo", "name":"volume", "device":"/dev/rbd0"},
            {"pool":"pool", "namespace":"", "name":"volume", "snap":"snapshot", "device":"/dev/rbd1"},
            {"pool":"pool", "namespace":"", "name":"volume", "snap":"-", "device":"/dev/rbd2"}
        ]);
        assert_eq!(
            match_rbd_head_device("pool/volume", &mappings).unwrap(),
            Some("/dev/rbd2".into())
        );
        assert!(
            match_rbd_head_device("pool/volume", &serde_json::json!([mappings[0].clone()]))
                .unwrap()
                .is_none()
        );
    }

    /// Exercises the product's Ceph RBD persistence helpers against a unique
    /// disposable image. Run only in the privileged `.local/dev` Odorobo
    /// container with `ODOROBO_ROOTFS_BACKEND=rbd` and a small test size.
    #[tokio::test]
    #[ignore = "requires privileged RBD access; uses and deletes a uniquely named disposable image"]
    async fn rbd_persistent_upper_cold_reattach_and_delete() {
        assert_eq!(
            std::env::var("ODOROBO_ROOTFS_BACKEND").as_deref(),
            Ok("rbd")
        );
        let vmid = ulid::Ulid::generate().to_string();
        let digest = format!("sha256:{}", "a".repeat(64));
        let mut image = None;
        for expected in [None, Some(b"survives reattach")] {
            let lock = acquire_rootfs_lock(&vmid).await.unwrap();
            let state = prepare_upper(
                &vmid,
                RootfsMode::Persistent,
                &digest,
                Path::new("/run/odorobo/test-runtime"),
            )
            .await
            .unwrap();
            let marker = state.upper.as_ref().unwrap().join("persistent-marker");
            if let Some(expected) = expected {
                assert_eq!(std::fs::read(marker).unwrap(), expected);
            } else {
                std::fs::write(marker, b"survives reattach").unwrap();
                image.clone_from(&state.rbd_image);
            }
            release_persistent_backend(state.state_mount.as_deref(), state.rbd_image.as_deref())
                .await
                .unwrap();
            drop(state);
            drop(lock);
        }
        delete_persistent_rootfs(&vmid).await.unwrap();
        let info = rbd_output(&["info".into(), image.unwrap()]).await.unwrap();
        assert!(
            !info.status.success(),
            "explicit delete must remove the per-VM RBD image"
        );
    }
}
