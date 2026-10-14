//! Shared fixtures for the container rootfs tests.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::Digest;

use super::oci::{OciLayerDescriptor, digest_blob_path, normalized_arch};
use super::process::Tool;
use super::to_hex;

/// Unique scratch directory, created on construction and removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        Self::under(&std::env::temp_dir(), prefix)
    }

    pub fn under(root: &Path, prefix: &str) -> Self {
        let path = root.join(format!("odorobo-{prefix}-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether every external tool can be spawned; tests skip otherwise.
pub fn require_tools(tools: &[&str]) -> bool {
    tools.iter().all(|tool| {
        std::process::Command::new(tool)
            .arg("--help")
            .output()
            .is_ok()
    })
}

/// Build an in-memory tar archive.
pub fn tar_bytes<F: FnOnce(&mut tar::Builder<&mut Vec<u8>>)>(build: F) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut tar = tar::Builder::new(&mut bytes);
    build(&mut tar);
    tar.finish().unwrap();
    drop(tar);
    bytes
}

/// A GNU header with the given type, mode, and owner; size is set on append.
pub fn header(entry_type: tar::EntryType, mode: u32, uid: u64, gid: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_mode(mode);
    header.set_uid(uid);
    header.set_gid(gid);
    header
}

pub fn append<W: Write>(
    tar: &mut tar::Builder<W>,
    mut header: tar::Header,
    path: &str,
    data: &[u8],
) {
    header.set_size(data.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, path, data).unwrap();
}

pub fn append_entry<W: Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    entry_type: tar::EntryType,
    mode: u32,
    uid: u64,
    gid: u64,
    data: &[u8],
) {
    append(tar, header(entry_type, mode, uid, gid), path, data);
}

fn put_blob(layout: &Path, bytes: &[u8]) -> String {
    let digest = format!("sha256:{}", to_hex(&sha2::Sha256::digest(bytes)));
    let path = digest_blob_path(layout, &digest).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    digest
}

/// Single-layer OCI layout with a multi-platform index, under `base/layout`.
#[derive(Clone, Copy)]
pub struct OciFixture<'a> {
    /// Byte filling the 16 KiB `/etc/x` payload.
    pub fill: u8,
    /// Whether the layer provides the `/run` scratch mount point.
    pub include_run: bool,
    /// Static binary installed as `/bin/sh`.
    pub init_binary: Option<&'a Path>,
}

impl Default for OciFixture<'_> {
    fn default() -> Self {
        Self {
            fill: b'x',
            include_run: true,
            init_binary: None,
        }
    }
}

impl OciFixture<'_> {
    pub fn layout(self, base: &Path) -> PathBuf {
        self.build(base).0
    }

    pub(super) fn build(self, base: &Path) -> (PathBuf, OciLayerDescriptor) {
        use tar::EntryType::{Directory, Regular};
        let layout = base.join("layout");
        std::fs::create_dir_all(&layout).unwrap();
        let tar_bytes = tar_bytes(|tar| {
            for (path, mode) in [
                ("etc/", 0o755),
                ("bin/", 0o755),
                ("tmp/", 0o1777),
                ("run/", 0o755),
                ("var/", 0o755),
                ("var/tmp/", 0o1777),
            ] {
                if (self.include_run || path != "run/")
                    && (self.init_binary.is_some() || path != "bin/")
                {
                    append_entry(tar, path, Directory, mode, 0, 0, b"");
                }
            }
            append_entry(tar, "etc/x", Regular, 0o644, 0, 0, &[self.fill; 16 * 1024]);
            if let Some(init_binary) = self.init_binary {
                let binary = std::fs::read(init_binary).unwrap();
                append_entry(tar, "bin/sh", Regular, 0o755, 0, 0, &binary);
            }
            append_entry(tar, "etc/.wh.deleted", Regular, 0o000, 0, 0, b"");
        });
        let layer_digest = put_blob(&layout, &tar_bytes);
        let diff_id = format!("sha256:{}", to_hex(&sha2::Sha256::digest(&tar_bytes)));
        let platform_arch = normalized_arch(std::env::consts::ARCH);
        let other_arch = if platform_arch == "amd64" {
            "arm64"
        } else {
            "amd64"
        };
        let config = serde_json::to_vec(&json!({"architecture": platform_arch, "os":"linux", "rootfs":{"type":"layers","diff_ids":[diff_id]}})).unwrap();
        let config_digest = put_blob(&layout, &config);
        let manifest = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config_digest,"size":config.len()},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":layer_digest,"size":tar_bytes.len()}]})).unwrap();
        let manifest_digest = put_blob(&layout, &manifest);
        let index = serde_json::to_vec(&json!({"schemaVersion":2,"manifests":[
            {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":manifest_digest,"size":manifest.len(),"platform":{"os":"linux","architecture":other_arch}},
            {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":manifest_digest,"size":manifest.len(),"platform":{"os":"linux","architecture":platform_arch},"annotations":{"org.opencontainers.image.ref.name":"latest"}}
        ]})).unwrap();
        std::fs::write(layout.join("index.json"), index).unwrap();
        std::fs::write(
            layout.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        (
            layout,
            OciLayerDescriptor {
                digest: layer_digest,
                diff_id,
                media_type: "application/vnd.oci.image.layer.v1.tar".into(),
                size: Some(tar_bytes.len() as u64),
            },
        )
    }
}

/// Default fixture layout and its single layer descriptor.
pub(super) fn fixture_oci_layout(base: &Path) -> (PathBuf, OciLayerDescriptor) {
    OciFixture::default().build(base)
}

/// `oci:` image reference for a fixture layout's `latest` tag.
pub fn oci_ref(layout: &Path) -> String {
    format!("oci:{}:latest", layout.display())
}

/// `mount -t <fstype> <fstype> [-o <options>] <target>`, ordered like product mounts.
pub(super) async fn mount(fstype: &str, options: Option<String>, target: &Path) {
    let mut args = vec!["-t".to_owned(), fstype.to_owned(), fstype.to_owned()];
    args.extend(
        options
            .into_iter()
            .flat_map(|options| ["-o".to_owned(), options]),
    );
    Tool::ORDERED.run("mount", args, &[target]).await.unwrap();
}

/// Detach `target`, failing if it is not mounted.
pub(super) async fn umount(target: &Path) {
    Tool::ORDERED.run("umount", [], &[target]).await.unwrap();
}

/// `sh -c script sh dir`, so the script addresses the test dir as `$1`.
pub fn sh_in(dir: &Path, script: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(script).arg("sh").arg(dir);
    command
}

/// Poll briefly until `path` exists.
pub async fn wait_for_path(path: &Path) {
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(path.exists(), "{} never appeared", path.display());
}
