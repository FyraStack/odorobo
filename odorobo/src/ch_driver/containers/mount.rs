//! Per-VM rootfs assembly: mounting the imported composefs image with a
//! per-VM overlay upper, scratch tmpfs mounts, and teardown.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use composefs::fsverity::{FsVerityHashValue, Sha256HashValue};
use stable_eyre::{
    Result,
    eyre::{Context, bail},
};
use tokio::process::Command;
use tracing::info;

use super::limits::{EPHEMERAL_STATE, MIB, RUN_SCRATCH, TMP_SCRATCH, VARTMP_SCRATCH};
use super::oci::OciUnpackLimits;
use super::persistent::{
    EPHEMERAL_STATE_DIR, PersistentBackend, acquire_rootfs_lock, persistent_backend, prepare_upper,
    rbd_identity, rbd_lookup_device, release_persistent_backend,
};
use super::process::{TOOL_OPERATIONS, Tool};
use super::repo;
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
    let image = prepare_image(rootfs).await?;
    let mut owner = None;
    match mount_rootfs_owned(vmid, rootfs, &image, &mut owner).await {
        Ok(()) => Ok(owner.unwrap()),
        Err(err) => {
            if let Some(prepared) = owner.as_ref() {
                unmount_rootfs(prepared).await.wrap_err("rootfs rollback")?;
            }
            Err(err)
        }
    }
}

/// An OCI image imported into the node-local composefs repository, ready to
/// mount for any VM.
#[derive(Debug)]
pub struct PreparedImage {
    digest: String,
    image: Sha256HashValue,
    config: Vec<u8>,
}

impl PreparedImage {
    /// Raw OCI image config document, delivered to the guest init.
    pub fn config(&self) -> &[u8] {
        &self.config
    }
}

/// Import the image into the shared composefs repository. Holds no per-VM
/// state, so it can run off the VM actor.
pub async fn prepare_image(rootfs: &Rootfs) -> Result<PreparedImage> {
    let limits = OciUnpackLimits::from_env()?;
    let imported = repo::import_image(&rootfs.oci, limits.max_compressed_bytes).await?;
    let (digest, image, config) = imported.into_parts();
    Ok(PreparedImage {
        digest,
        image,
        config,
    })
}

/// Publish acquired resources to a lifecycle owner before the first mount.
/// A failed startup retains this inventory for an explicit teardown retry.
pub async fn mount_rootfs_owned(
    vmid: &str,
    rootfs: &Rootfs,
    image: &PreparedImage,
    owner: &mut Option<PreparedRootfs>,
) -> Result<()> {
    validate_vmid(vmid)?;
    let state_lock = acquire_rootfs_lock(vmid).await?;
    let manifest_digest = &image.digest;

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
    *owner = Some(PreparedRootfs {
        digest: manifest_digest.clone(),
        mount: root_mount.clone(),
        upper: None,
        work: None,
        persistent_state_mount: None,
        rbd_image: None,
        _state_lock: state_lock,
        mode: rootfs.mode,
        scratch_mounts: Vec::new(),
        // Recorded before prepare_upper mounts it, so a cancelled or failed
        // start can still be torn down through the owner.
        ephemeral_state_mount: ephemeral_state,
    });
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
    let upper_state = prepare_upper(vmid, rootfs.mode, manifest_digest, &runtime_dir).await?;
    {
        let prepared = owner.as_mut().unwrap();
        prepared.upper.clone_from(&upper_state.upper);
        prepared.work.clone_from(&upper_state.work);
    };
    let overlay = match (&upper_state.upper, &upper_state.work) {
        (Some(upper), Some(work)) => Some((upper.as_path(), work.as_path())),
        _ => None,
    };
    repo::mount_image(&image.image, &root_mount, overlay)?;
    Tool::ORDERED
        .run(
            "mount",
            ["--make-rprivate".to_owned()],
            &[root_mount.as_path()],
        )
        .await?;
    // composefs mounts carry only the read-only attribute; keep the root as
    // restricted as the overlay options it replaces.
    Tool::ORDERED
        .run(
            "mount",
            ["-o".into(), "remount,bind,nosuid,nodev".into()],
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
    info!(vmid, %manifest_digest, image_object = %image.image.to_hex(), mount=%root_mount.display(), "OCI rootfs mounted from the composefs repository");
    owner.as_mut().unwrap().scratch_mounts = scratch_mounts;
    Ok(())
}

/// Tear down mounts in dependency order. Persistent upper data is never
/// removed here; RBD is unmounted and unmapped only after the overlay stops.
pub async fn unmount_rootfs(prepared: &PreparedRootfs) -> Result<()> {
    unmount_scratch_mounts(&prepared.scratch_mounts).await?;
    unmount_path(&prepared.mount).await?;
    if let Some(state) = &prepared.ephemeral_state_mount {
        unmount_path(state)
            .await
            .wrap_err("ephemeral rootfs state unmount failed")?;
    }
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
    #[ignore = "requires privileged tmpfs mounts; tests busy-root rollback and purge protection"]
    async fn partial_teardown_retries_without_purging_live_mounts() {
        let vmid = ulid::Ulid::generate().to_string();
        let mut instance = VMInstance::new(&vmid, PathBuf::from("/unused"), None, None);
        let runtime = instance.runtime_dir();
        let root = runtime.join("rootfs");
        let state = runtime.join(EPHEMERAL_STATE_DIR);
        let upper = state.join("rootfs.upper");
        for path in [&root, &state] {
            std::fs::create_dir_all(path).unwrap();
            mount("tmpfs", None, path).await;
        }
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::write(upper.join("keep"), b"persistent until detached").unwrap();
        let mut busy = hold_busy(&root);
        let prepared = PreparedRootfs {
            digest: "fixture".into(),
            mount: root.clone(),
            upper: Some(upper.clone()),
            work: None,
            persistent_state_mount: None,
            rbd_image: None,
            _state_lock: acquire_rootfs_lock(&vmid).await.unwrap(),
            mode: RootfsMode::Ephemeral,
            scratch_mounts: vec![],
            ephemeral_state_mount: Some(state.clone()),
        };
        async {
            assert!(unmount_rootfs(&prepared).await.is_err());
            assert!(is_mountpoint(&root).await.unwrap());
            assert!(
                is_mountpoint(&state).await.unwrap(),
                "ephemeral state must outlive a failed root teardown"
            );
            assert!(instance.purge_instance_data().is_err());
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
    async fn oci_rootfs_modes_keep_base_and_writes_isolated() {
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
            "whiteouts must be applied when the image is imported"
        );
        unmount_rootfs(&ro_a).await.unwrap();
        unmount_rootfs(&ro_b).await.unwrap();
        drop(ro_a);
        drop(ro_b);

        // A late missing scratch path must fail without leaving a /tmp
        // tmpfs or the root mount behind.
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
        let image = prepare_image(&rootfs).await.unwrap();
        assert!(
            mount_rootfs_owned(&vmid, &rootfs, &image, &mut owner)
                .await
                .is_err()
        );
        let prepared = owner
            .as_ref()
            .expect("failed acquisition must retain ownership");
        assert!(is_mountpoint(&prepared.mount).await.unwrap());
        let _err = acquire_rootfs_lock(&vmid).await.unwrap_err();
        let mut busy = hold_busy(&prepared.mount);
        assert!(unmount_rootfs(prepared).await.is_err());
        let _err = acquire_rootfs_lock(&vmid).await.unwrap_err();
        busy.kill().await.unwrap();
        unmount_rootfs(prepared).await.unwrap();
        drop(owner);
        acquire_rootfs_lock(&vmid).await.unwrap();
        std::fs::remove_dir_all(VMInstance::runtime_dir_for(&vmid)).unwrap();
    }
}
