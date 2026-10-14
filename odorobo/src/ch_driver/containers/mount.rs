//! Per-VM rootfs assembly: composefs layer mounts, `OverlayFS` stacking,
//! scratch tmpfs mounts, and teardown.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use stable_eyre::{
    Result,
    eyre::{Context, bail, eyre},
};
use tokio::process::Command;
use tracing::info;

use super::dirmeta::{DIRECTORY_OPERATIONS, compose_directory_metadata};
use super::layer_store::materialize_layer;
use super::limits::{EPHEMERAL_STATE, MIB, RUN_SCRATCH, TMP_SCRATCH, VARTMP_SCRATCH};
use super::oci::{OciUnpackLimits, pull_oci, read_oci_manifest};
use super::persistent::{
    EPHEMERAL_STATE_DIR, PersistentBackend, acquire_rootfs_lock, persistent_backend, prepare_upper,
    rbd_identity, rbd_lookup_device, release_persistent_backend,
};
use super::process::{TOOL_OPERATIONS, Tool};
use super::{PERSISTENT_ROOTFS_ROOT, PreparedRootfs, ROOTFS_TAG, validate_vmid};
use crate::manifest::{Rootfs, RootfsMode};

pub(super) async fn is_mountpoint(path: &Path) -> Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
        Ok(_) => {}
    }
    let mut command = Command::new("mountpoint");
    command.arg("-q").arg(path);
    let output = Tool::bounded(Duration::from_secs(30))
        .output(command)
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

/// Idempotence is essential: teardown may have succeeded only partially on
/// its previous attempt. Never treat an already-detached mount as a failure.
pub(super) async fn unmount_path(path: &Path) -> Result<()> {
    // An acquisition may outlive a cancelled caller; observe its completion
    // before checking whether the target needs detaching.
    {
        let _barrier = TOOL_OPERATIONS.lock().await;
    }
    if is_mountpoint(path).await? {
        Tool::ORDERED.run("umount", [], &[path]).await?;
    }
    Ok(())
}

pub(super) async fn unmount_layer_mounts(mounts: &[PathBuf]) -> Result<()> {
    for path in mounts.iter().rev() {
        unmount_path(path).await?;
    }
    Ok(())
}

/// Prepare a digest-addressed composefs mount for each OCI layer, then expose
/// their OCI-ordered `OverlayFS` stack as the VM root. No flattened image copy is
/// retained; only per-layer stores and per-reference OCI layouts are cached.
#[cfg(test)]
pub(super) async fn mount_rootfs(vmid: &str, rootfs: &Rootfs) -> Result<PreparedRootfs> {
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
#[expect(
    clippy::too_many_lines,
    reason = "sequential setup with interleaved cleanup; splitting would obscure ownership"
)]
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
    tokio::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700)).await?;
    let root_mount = runtime_dir.join(ROOTFS_TAG);
    tokio::fs::create_dir_all(&root_mount).await?;
    if is_mountpoint(&root_mount).await? {
        bail!(
            "rootfs mountpoint {} is already mounted",
            root_mount.display()
        );
    }
    // Refuse to adopt a pre-existing ephemeral state mount, like the root.
    let ephemeral_state =
        (rootfs.mode == RootfsMode::Ephemeral).then(|| runtime_dir.join(EPHEMERAL_STATE_DIR));
    if let Some(state) = &ephemeral_state
        && is_mountpoint(state).await?
    {
        bail!(
            "ephemeral rootfs state {} is already mounted",
            state.display()
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
        // Recorded before prepare_upper mounts it, so a cancelled or failed
        // start can still be torn down through the owner.
        ephemeral_state_mount: ephemeral_state,
    });
    let mut lowerdirs = Vec::with_capacity(mount_specs.len());
    for (_layer_digest, cache, verity_digest, layer_mount) in &mount_specs {
        let opts = format!(
            "basedir={},digest={},verity,ro",
            cache.join("store").display(),
            verity_digest
        );
        Tool::ORDERED
            .run(
                "mount.composefs",
                ["-o".to_owned(), opts],
                &[cache.join("layer.cfs").as_path(), layer_mount.as_path()],
            )
            .await?;
        Tool::ORDERED
            .run(
                "mount",
                ["--make-rprivate".to_owned()],
                &[layer_mount.as_path()],
            )
            .await?;
        // mount.composefs does not accept standard nosuid/nodev options;
        // apply these host-side restrictions to the resulting mount instead.
        Tool::ORDERED
            .run(
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
        prepared.upper.clone_from(&upper_state.upper);
        prepared.work.clone_from(&upper_state.work);
    };
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
        use std::fmt::Write as _;
        write!(
            options,
            ",upperdir={},workdir={}",
            upper.display(),
            work.display()
        )?;
    } else {
        options.push_str(",ro");
    }
    if options.len() >= 4096 {
        bail!("OCI OverlayFS mount options exceed the kernel page-size limit");
    }
    Tool::ORDERED
        .run(
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
    Tool::ORDERED
        .run(
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

/// Tear down mounts in dependency order. Persistent upper data is never removed
/// here; RBD is unmounted and unmapped only after the overlay and layers stop.
pub async fn unmount_rootfs(prepared: &PreparedRootfs) -> Result<()> {
    let directory_barrier = DIRECTORY_OPERATIONS.lock().await;
    unmount_scratch_mounts(&prepared.scratch_mounts).await?;
    unmount_path(&prepared.mount).await?;
    unmount_layer_mounts(&prepared.layer_mounts).await?;
    if let Some(state) = &prepared.ephemeral_state_mount {
        unmount_path(state)
            .await
            .wrap_err("ephemeral rootfs state unmount failed")?;
    }
    if let Some(runtime) = prepared.mount.parent().and_then(Path::file_name) {
        let directory = Path::new("/var/lib/odorobo/directory-metadata").join(runtime);
        match tokio::fs::remove_dir_all(directory).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    // The backend lock can be held for a long persistent attach elsewhere;
    // don't block other VMs' directory-metadata composition meanwhile.
    drop(directory_barrier);
    release_persistent_backend(
        prepared.persistent_state_mount.as_deref(),
        prepared.rbd_image.as_deref(),
    )
    .await?;
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

/// Mount a fresh VM-private tmpfs for ephemeral `OverlayFS` upper/work state.
/// Guest writes are bounded by its `size=` instead of consuming host /run.
pub(super) async fn mount_ephemeral_state(path: &Path) -> Result<()> {
    let options = format!(
        "size={}m,mode=700,nosuid,nodev",
        EPHEMERAL_STATE.read_env()? / MIB
    );
    tokio::fs::create_dir_all(path).await?;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    if is_mountpoint(path).await? {
        bail!(
            "ephemeral rootfs state {} is already mounted",
            path.display()
        );
    }
    Tool::ORDERED
        .run(
            "mount",
            [
                "-t".into(),
                "tmpfs".into(),
                "-o".into(),
                options,
                "tmpfs".into(),
            ],
            &[path],
        )
        .await?;
    Tool::ORDERED
        .run("mount", ["--make-private".to_owned()], &[path])
        .await?;
    Ok(())
}

/// Resolve the three scratch locations completely before mounting any tmpfs.
/// This avoids leaving an earlier scratch mount behind when a later path is
/// absent, a symlink, or not a directory in the immutable image.
pub(super) fn validate_scratch_targets(root: &Path) -> Result<Vec<(PathBuf, String)>> {
    let paths = [
        (
            "tmp",
            format!(
                "size={}m,mode=1777,nosuid,nodev",
                TMP_SCRATCH.read_env()? / MIB
            ),
        ),
        (
            "run",
            format!(
                "size={}m,mode=755,nosuid,nodev",
                RUN_SCRATCH.read_env()? / MIB
            ),
        ),
        (
            "var/tmp",
            format!(
                "size={}m,mode=1777,nosuid,nodev",
                VARTMP_SCRATCH.read_env()? / MIB
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

pub(super) async fn unmount_scratch_mounts(mounts: &[PathBuf]) -> Result<()> {
    unmount_layer_mounts(mounts)
        .await
        .wrap_err("scratch unmount failed")
}

/// Read-only root supports only bounded volatile scratch: /tmp, /run, /var/tmp.
/// Sizes are node policy via `ODOROBO_ROOTFS_{TMP,RUN,VARTMP}_SIZE` (M/G).
pub(super) async fn mount_readonly_scratch(root: &Path) -> Result<Vec<PathBuf>> {
    let targets = validate_scratch_targets(root)?;
    // Private propagation prevents nested per-VM tmpfs mounts escaping through
    // shared host mount trees or appearing in another VM's mount view.
    Tool::ORDERED
        .run("mount", ["--make-rprivate".to_owned()], &[root])
        .await?;
    let mut mounted = Vec::with_capacity(targets.len());
    for (target, options) in targets {
        if let Err(err) = Tool::ORDERED
            .run(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ch_driver::VMInstance;
    use crate::ch_driver::containers::persistent::delete_persistent_rootfs;
    use crate::ch_driver::containers::test_support::{
        OciFixture, TempDir, mount, oci_ref, sh_in, wait_for_path,
    };

    async fn mount_image(vmid: &str, oci: &str, mode: RootfsMode) -> Result<PreparedRootfs> {
        mount_rootfs(
            vmid,
            &Rootfs {
                oci: oci.to_owned(),
                mode,
            },
        )
        .await
    }

    /// Spawn a process holding `dir` busy so it cannot be unmounted.
    fn hold_busy(dir: &Path) -> tokio::process::Child {
        Command::new("sleep")
            .arg("100")
            .current_dir(dir)
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn cancelled_mount_helper_descendant_cannot_overtake_teardown() {
        let base = TempDir::new("mount-cancel");
        let caller = tokio::spawn(Tool::ORDERED.output(sh_in(
            &base,
            "(echo ready > \"$1/ready\"; sleep 0.2; echo acquired > \"$1/acquired\") & exit 0",
        )));
        wait_for_path(&base.join("ready")).await;
        caller.abort();
        _ = caller.await;
        unmount_path(&base).await.unwrap();
        assert!(
            base.join("acquired").exists(),
            "teardown must wait for helper descendants"
        );
        std::fs::remove_file(base.join("acquired")).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!base.join("acquired").exists());
    }

    #[tokio::test]
    async fn teardown_accepts_already_unmounted_paths() {
        let base = TempDir::new("teardown");
        unmount_layer_mounts(&[base.to_path_buf(), base.join("missing")])
            .await
            .unwrap();
        for _ in 0..2 {
            release_persistent_backend(Some(&base), None).await.unwrap();
        }
    }

    #[test]
    fn scratch_paths_are_prevalidated_before_any_mounting() {
        let base = TempDir::new("scratch-preflight");
        std::fs::create_dir_all(base.join("tmp")).unwrap();
        let before = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        assert!(
            validate_scratch_targets(&base).is_err(),
            "missing /run and /var/tmp must fail preflight"
        );
        assert_eq!(
            before,
            std::fs::read_to_string("/proc/self/mountinfo").unwrap(),
            "preflight must not mount the valid earlier /tmp path"
        );
        std::fs::create_dir_all(base.join("var/tmp")).unwrap();
        std::os::unix::fs::symlink("/tmp", base.join("run")).unwrap();
        assert!(
            validate_scratch_targets(&base).is_err(),
            "symlink scratch targets must reject"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires privileged tmpfs mounts; tests busy-layer rollback and purge protection"]
    async fn partial_teardown_retries_without_purging_live_mounts() {
        let vmid = ulid::Ulid::generate().to_string();
        let mut instance = VMInstance::new(&vmid, PathBuf::from("/unused"), None, None);
        let runtime = instance.runtime_dir();
        let root = runtime.join("rootfs");
        let layer = runtime.join("rootfs-layers/0");
        let state = runtime.join(EPHEMERAL_STATE_DIR);
        let upper = state.join("rootfs.upper");
        for path in [&root, &layer, &state] {
            std::fs::create_dir_all(path).unwrap();
            mount("tmpfs", None, path).await;
        }
        std::fs::write(layer.join("keep"), b"never purge through a mount").unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::write(upper.join("keep"), b"persistent until detached").unwrap();
        let mut busy = hold_busy(&layer);
        let prepared = PreparedRootfs {
            digest: "fixture".into(),
            mount: root.clone(),
            upper: Some(upper.clone()),
            work: None,
            layer_mounts: vec![layer.clone()],
            persistent_state_mount: None,
            rbd_image: None,
            _state_lock: acquire_rootfs_lock(&vmid).await.unwrap(),
            mode: RootfsMode::Ephemeral,
            scratch_mounts: vec![],
            ephemeral_state_mount: Some(state.clone()),
        };
        async {
            assert!(unmount_rootfs(&prepared).await.is_err());
            assert!(!is_mountpoint(&root).await.unwrap());
            assert!(is_mountpoint(&layer).await.unwrap());
            assert!(
                is_mountpoint(&state).await.unwrap(),
                "ephemeral state must outlive a failed lower teardown"
            );
            assert!(instance.purge_instance_data().is_err());
            assert!(layer.join("keep").exists());
            assert!(upper.join("keep").exists());
            busy.kill().await.unwrap();
            unmount_rootfs(&prepared).await.unwrap();
            unmount_rootfs(&prepared).await.unwrap();
            assert!(!is_mountpoint(&state).await.unwrap());
            assert!(!upper.exists());
            instance.purge_instance_data().unwrap();
        }
        .await;
        _ = busy.kill().await;
        _ = unmount_rootfs(&prepared).await;
        _ = instance.purge_instance_data();
    }

    #[tokio::test]
    #[ignore = "requires skopeo, composefs/OverlayFS mounts, fs-verity, and a privileged host mount namespace"]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end test exercises one long scenario"
    )]
    async fn oci_rootfs_modes_share_verified_layers_and_keep_writes_isolated() {
        let base = TempDir::under(Path::new("/var/lib/odorobo/oci-rootfs-tests"), "modes");
        let image = oci_ref(&OciFixture::default().layout(&base.join("base")));
        let changed_image = oci_ref(
            &OciFixture {
                fill: b'y',
                ..OciFixture::default()
            }
            .layout(&base.join("changed")),
        );
        let [
            ephemeral_a,
            ephemeral_b,
            readonly_a,
            readonly_b,
            failed_scratch,
            persistent,
        ] = std::array::from_fn(|_| ulid::Ulid::generate().to_string());

        let a = mount_image(&ephemeral_a, &image, RootfsMode::Ephemeral)
            .await
            .unwrap();
        let b = mount_image(&ephemeral_b, &image, RootfsMode::Ephemeral)
            .await
            .unwrap();
        std::fs::write(a.mount.join("etc/only-a"), b"a").unwrap();
        let a_state = VMInstance::runtime_dir_for(&ephemeral_a).join(EPHEMERAL_STATE_DIR);
        assert!(
            is_mountpoint(&a_state).await.unwrap()
                && a.upper.as_ref().unwrap().starts_with(&a_state),
            "ephemeral upper must live on its bounded VM-private tmpfs"
        );
        assert!(
            !b.mount.join("etc/only-a").exists(),
            "ephemeral upper must be VM-private"
        );
        unmount_rootfs(&a).await.unwrap();
        unmount_rootfs(&b).await.unwrap();
        assert!(!is_mountpoint(&a_state).await.unwrap());
        drop(a);
        drop(b);
        let a = mount_image(&ephemeral_a, &image, RootfsMode::Ephemeral)
            .await
            .unwrap();
        assert!(
            !a.mount.join("etc/only-a").exists(),
            "ephemeral writes must disappear at stop"
        );
        unmount_rootfs(&a).await.unwrap();
        drop(a);

        let ro_a = mount_image(&readonly_a, &image, RootfsMode::ReadOnly)
            .await
            .unwrap();
        let ro_b = mount_image(&readonly_b, &image, RootfsMode::ReadOnly)
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
        unmount_rootfs(&ro_a).await.unwrap();
        unmount_rootfs(&ro_b).await.unwrap();
        drop(ro_a);
        drop(ro_b);

        // A late missing scratch path must fail without leaving a /tmp
        // tmpfs, the root bind, or its composefs layer mounted.
        let incomplete = OciFixture {
            fill: b'z',
            include_run: false,
            ..OciFixture::default()
        }
        .layout(&base.join("missing-scratch"));
        let _err = mount_image(&failed_scratch, &oci_ref(&incomplete), RootfsMode::ReadOnly)
            .await
            .unwrap_err();
        let failed_runtime = VMInstance::runtime_dir_for(&failed_scratch);
        assert!(
            !is_mountpoint(&failed_runtime.join(ROOTFS_TAG))
                .await
                .unwrap()
        );
        for entry in std::fs::read_dir(failed_runtime.join("rootfs-layers")).unwrap() {
            assert!(!is_mountpoint(&entry.unwrap().path()).await.unwrap());
        }

        let p = mount_image(&persistent, &image, RootfsMode::Persistent)
            .await
            .unwrap();
        std::fs::write(p.mount.join("etc/survives"), b"persist").unwrap();
        unmount_rootfs(&p).await.unwrap();
        drop(p);
        assert!(
            mount_image(&persistent, &changed_image, RootfsMode::Persistent)
                .await
                .is_err(),
            "changing the base digest must be rejected"
        );
        let p = mount_image(&persistent, &image, RootfsMode::Persistent)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(p.mount.join("etc/survives")).unwrap(),
            b"persist"
        );
        unmount_rootfs(&p).await.unwrap();
        drop(p);
        delete_persistent_rootfs(&persistent).await.unwrap();
        for vmid in [
            ephemeral_a,
            ephemeral_b,
            readonly_a,
            readonly_b,
            failed_scratch,
            persistent,
        ] {
            _ = std::fs::remove_dir_all(VMInstance::runtime_dir_for(&vmid));
        }
    }

    #[tokio::test]
    #[ignore = "requires privileged composefs mounts; failed scratch setup retains resource owner"]
    async fn failed_rootfs_start_retains_mount_inventory_for_retry() {
        let base = TempDir::under(Path::new("/var/lib/odorobo/startup-owner-tests"), "owner");
        let layout = OciFixture {
            include_run: false,
            ..OciFixture::default()
        }
        .layout(&base);
        let vmid = ulid::Ulid::generate().to_string();
        let mut owner = None;
        let rootfs = Rootfs {
            oci: oci_ref(&layout),
            mode: RootfsMode::ReadOnly,
        };
        assert!(
            mount_rootfs_owned(&vmid, &rootfs, &mut owner)
                .await
                .is_err()
        );
        let prepared = owner
            .as_ref()
            .expect("failed acquisition must retain ownership");
        assert!(is_mountpoint(&prepared.mount).await.unwrap());
        let _err = acquire_rootfs_lock(&vmid).await.unwrap_err();
        let mut busy = hold_busy(&prepared.layer_mounts[0]);
        assert!(unmount_rootfs(prepared).await.is_err());
        let _err = acquire_rootfs_lock(&vmid).await.unwrap_err();
        busy.kill().await.unwrap();
        unmount_rootfs(prepared).await.unwrap();
        drop(owner);
        acquire_rootfs_lock(&vmid).await.unwrap();
        std::fs::remove_dir_all(VMInstance::runtime_dir_for(&vmid)).unwrap();
    }
}
