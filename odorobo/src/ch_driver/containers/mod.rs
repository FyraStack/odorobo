//! OCI container microVM roots served over virtiofs. All OCI, composefs,
//! `OverlayFS`, fs-verity, and persistence preparation happens on the host; the
//! guest sees an ordinary virtiofs root and does not need composefs support.
//!
//! Skopeo layouts keep per-reference metadata while verified compressed blobs
//! are hard-linked into a node-local digest CAS. Each unique OCI layer is
//! extracted temporarily, translated to `OverlayFS` whiteout semantics, and
//! built as an immutable fs-verity composefs image with a per-layer object
//! store. Temporary extraction is deleted. Per-VM composefs mounts are stacked
//! in OCI order with `OverlayFS` before actor-supervised virtiofsd exports them.
//! Security notes: layers are unpacked by this process as root; tar paths
//! are validated (`..` rejected) and every unpack/whiteout operation refuses
//! to traverse symlinks planted by earlier layers, so a hostile image cannot
//! write through or remove host paths. The store must live on an
//! fs-verity-capable filesystem (ext4, btrfs) for digest-pinned mounts.

mod dirmeta;
mod layer_store;
mod limits;
mod mount;
mod oci;
mod persistent;
mod process;
#[cfg(test)]
pub mod test_support;
mod unpack;
mod virtiofs;

pub use mount::{mount_rootfs_owned, unmount_rootfs};
pub use persistent::{PERSISTENT_OPERATIONS, RBD_RESERVATIONS, delete_persistent_rootfs};
pub use virtiofs::{VirtioFsSupervisor, VirtiofsdSpec, socket_path_for, virtiofsd_path};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use stable_eyre::{Result, eyre::bail};

use crate::manifest::RootfsMode;

/// virtiofs mount tag the guest root is served under.
pub const ROOTFS_TAG: &str = "rootfs";

/// The tiny custom microvm kernel built by
/// `scripts/build-microvm-kernel.sh` (issue #112 buildconfig).
pub const MICROVM_KERNEL_PATH: &str = "/var/lib/odorobo/microvm/vmlinux";

/// Direct-kernel-boot cmdline for a composefs/virtiofs root. The guest init
/// must live at /sbin/init, /etc/init, /bin/init or /bin/sh (kernel default).
/// console=ttyS0 (the 16550 UART) because the driver's `ConsoleTransform`
/// sockets the UART; the virtio-console (hvc0) is off.
pub const DEFAULT_ROOTFS_CMDLINE: &str = "console=ttyS0 root=rootfs rootfstype=virtiofs rw";

const OCI_CACHE_ROOT: &str = "/var/lib/odorobo/oci-cache";

const OCI_BLOBS_ROOT: &str = "/var/lib/odorobo/oci-blobs/sha256";

const PERSISTENT_ROOTFS_ROOT: &str = "/var/lib/odorobo/persistent-rootfs";

/// Remove temporary external-tool artifacts even if their async operation is
/// cancelled. Large directory cleanup is moved to Tokio's blocking pool.
struct RemovePathOnDrop(Option<PathBuf>);

impl RemovePathOnDrop {
    const fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for RemovePathOnDrop {
    fn drop(&mut self) {
        let Some(path) = self.0.take() else {
            return;
        };
        let cleanup = move || match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            _ = handle.spawn_blocking(cleanup);
        } else {
            _ = cleanup();
        }
    }
}

/// A rootfs made ready for a specific VM.
#[derive(Debug)]
pub struct PreparedRootfs {
    /// OCI manifest digest identifying the immutable base.
    pub digest: String,
    /// This VM's mount point of the rootfs (virtiofsd shared-dir).
    pub mount: PathBuf,
    /// Per-VM overlayfs upper directory when the root is writable.
    pub upper: Option<PathBuf>,
    /// Per-VM overlayfs work directory when the root is writable.
    pub work: Option<PathBuf>,
    /// Per-layer composefs mounts backing the merged `OverlayFS` lower.
    layer_mounts: Vec<PathBuf>,
    /// Persistent backend mount directory; populated only for RBD state.
    persistent_state_mount: Option<PathBuf>,
    /// RBD image identity for explicit deletion.
    rbd_image: Option<String>,
    /// Node-local flock prevents duplicate actor instances for this VM id.
    _state_lock: Arc<std::fs::File>,
    /// Persistence policy controls cleanup and writable scratch behavior.
    pub mode: RootfsMode,
    /// Host tmpfs mounts used for the bounded read-only-mode scratch paths.
    scratch_mounts: Vec<PathBuf>,
    /// VM-private size-bounded tmpfs holding the ephemeral upper and work.
    ephemeral_state_mount: Option<PathBuf>,
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
        out
    })
}

fn remove_path(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

fn validate_vmid(vmid: &str) -> Result<()> {
    if vmid.is_empty() || !vmid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("unsafe VM id for rootfs state path");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_builder_does_not_remove_a_live_staging_writer() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let path = std::env::temp_dir().join(format!(
            "odorobo-extraction-cancel-{}",
            ulid::Ulid::generate()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let cleanup = RemovePathOnDrop::new(path.clone());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let writer_path = path.clone();
        let builder = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let cleanup = cleanup;
                started_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                std::fs::create_dir_all(writer_path.join("tree/children")).unwrap();
                std::fs::write(
                    writer_path.join("tree/children/payload"),
                    b"privileged payload",
                )
                .unwrap();
                ((), cleanup)
            })
            .await
            .unwrap()
        });
        started_rx.await.unwrap();
        builder.abort();
        _ = builder.await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        resume_tx.send(()).unwrap();
        for _ in 0..100 {
            if !path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!path.exists(), "last writer must remove abandoned staging");
    }

    #[test]
    fn temporary_pull_guard_removes_cancelled_artifact_path() {
        let dir = test_support::TempDir::new("cancel-cleanup");
        std::fs::write(dir.join("partial"), b"partial").unwrap();
        drop(RemovePathOnDrop::new(dir.to_path_buf()));
        assert!(!dir.exists());
    }
}
