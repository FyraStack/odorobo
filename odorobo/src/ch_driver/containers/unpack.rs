//! Bounded OCI layer tar extraction: path validation, symlink-safe writes,
//! whiteout translation to `OverlayFS` markers, and metadata restoration.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::Digest;
use stable_eyre::{
    Result,
    eyre::{Context, bail, eyre},
};
use tracing::{debug, warn};

use super::{remove_path, to_hex};

/// Media type of an OCI layer blob -> how it is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Compression {
    Gzip,
    Zstd,
    None,
}

pub(super) fn compression_of_media_type(media_type: &str) -> Option<Compression> {
    // OCI and Docker layer media types; tar is the uncompressed variant.
    const TAR: [&str; 2] = [
        "application/vnd.oci.image.layer.v1.tar",
        "application/vnd.docker.image.rootfs.diff.tar",
    ];
    const GZIP: [&str; 2] = [
        "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.docker.image.rootfs.diff.tar.gzip",
    ];
    const ZSTD: [&str; 2] = [
        "application/vnd.oci.image.layer.v1.tar+zstd",
        "application/vnd.docker.image.rootfs.diff.tar.zstd",
    ];
    if TAR.contains(&media_type) {
        Some(Compression::None)
    } else if GZIP.contains(&media_type) {
        Some(Compression::Gzip)
    } else if ZSTD.contains(&media_type) {
        Some(Compression::Zstd)
    } else {
        None
    }
}

/// Reject tar paths that could escape the scratch tree. `.` components are
/// harmless (GNU tar layers often start with `./`) and are skipped; only
/// `..` (and prefixes) are rejected.
pub(super) fn safe_rel_path(raw: &Path) -> Option<PathBuf> {
    let stripped = raw.strip_prefix("/").unwrap_or(raw);
    let mut clean = PathBuf::new();
    for component in stripped.components() {
        match component {
            std::path::Component::Normal(part) => clean.push(part),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(clean)
}

/// Every prefix of `rel` under `tree` must exist and not be a symlink. Tar
/// layers can plant symlinks (`etc -> /etc`); without this guard, directory
/// creation, opaque-whiteout scans and unpacking would resolve through them
/// on the HOST and a hostile image could touch host paths.
pub(super) fn ensure_no_symlink_ancestors(tree: &Path, rel: &Path) -> Result<()> {
    let mut cur = tree.to_path_buf();
    for component in rel.components() {
        cur.push(component);
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) if meta.is_symlink() => {
                bail!(
                    "tar path {rel:?} traverses symlink {} — refusing (hostile layer?)",
                    cur.display()
                );
            }
            Ok(_) => {}
            // Missing ancestor: nothing below it can be a symlink.
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

/// One OCI whiteout marker deferred until all normal entries in the same layer
/// have been extracted. This makes whiteout behavior independent of tar order.
pub(super) struct Whiteout {
    pub parent: PathBuf,
    pub file_name: String,
}

/// Preserve whiteouts in layer form. `OverlayFS` supports the trusted.overlay
/// xattr encoding as well as char-device whiteouts; the xattr form works on
/// filesystems and container policies where creating device nodes is forbidden.
pub(super) fn apply_whiteout(tree: &Path, parent_rel: &Path, file_name: &str) -> Result<()> {
    ensure_no_symlink_ancestors(tree, parent_rel)?;
    let parent = tree.join(parent_rel);
    std::fs::create_dir_all(&parent)
        .wrap_err_with(|| format!("create whiteout parent {}", parent.display()))?;
    if file_name == ".wh..wh..opq" {
        set_overlay_xattr(&parent, "trusted.overlay.opaque", b"y")?;
        debug!(dir = %parent.display(), "preserved opaque whiteout");
    } else {
        let target_name = file_name.strip_prefix(".wh.").unwrap_or(file_name);
        if target_name.is_empty() || target_name == "." || target_name == ".." {
            bail!("invalid OCI whiteout name {file_name:?}");
        }
        let target = parent.join(target_name);
        if std::fs::symlink_metadata(&target).is_ok() {
            bail!(
                "refusing to replace same-layer path with whiteout {}",
                target.display()
            );
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o000)
            .open(&target)
            .wrap_err_with(|| {
                format!("create regular-file overlay whiteout {}", target.display())
            })?;
        set_overlay_xattr(&target, "trusted.overlay.whiteout", b"")?;
        // The opaque=x marker means "this merge dir contains xattr whiteouts"
        // (not opaque=y); OverlayFS uses it to detect these regular files.
        set_overlay_xattr(&parent, "trusted.overlay.opaque", b"x")?;
        debug!(target = %target.display(), "preserved xattr overlay whiteout");
    }
    Ok(())
}

pub(super) fn set_overlay_xattr(path: &Path, attribute: &str, value: &[u8]) -> Result<()> {
    xattr::set(path, attribute, value)
        .wrap_err_with(|| format!("set {attribute} on {}", path.display()))
}

/// Unpack one non-whiteout tar entry. Whiteouts are returned for deferred
/// application after all files from this same OCI layer have been extracted.
pub(super) struct DirectoryMetadata {
    pub relative: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub mtime: u64,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

pub(super) fn apply_xattrs(target: &Path, xattrs: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    for (name, value) in xattrs {
        // xattr::set uses lsetxattr: symlinks are never followed.
        xattr::set(target, std::ffi::OsStr::from_bytes(name), value)
            .wrap_err("preserve OCI xattr")?;
    }
    Ok(())
}

pub(super) fn restore_directory_metadata(
    tree: &Path,
    directories: &mut [DirectoryMetadata],
) -> Result<()> {
    // Descendants first: default ACLs must not affect extraction, and final
    // directory modes/times must not be disturbed by children or whiteouts.
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.relative.components().count()));
    for directory in directories {
        ensure_no_symlink_ancestors(tree, &directory.relative)?;
        let path = tree.join(&directory.relative);
        if !std::fs::symlink_metadata(&path)?.is_dir() {
            bail!(
                "OCI directory replaced by non-directory: {}",
                path.display()
            );
        }
        let file = std::fs::File::open(&path)?;
        // SAFETY: live fd and checked numeric ownership; no path following.
        if unsafe { libc::fchown(file.as_raw_fd(), directory.uid, directory.gid) } != 0 {
            return Err(std::io::Error::last_os_error()).wrap_err("preserve directory ownership");
        }
        file.set_permissions(std::fs::Permissions::from_mode(directory.mode))?;
        file.set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH
                    .checked_add(Duration::from_secs(directory.mtime))
                    .ok_or_else(|| eyre!("directory mtime out of range"))?,
            ),
        )?;
        apply_xattrs(&path, &directory.xattrs)?;
    }
    Ok(())
}

pub(super) fn unpack_entry<R: Read>(
    tree: &Path,
    mut entry: tar::Entry<'_, R>,
    directories: &mut Vec<DirectoryMetadata>,
) -> Result<Option<Whiteout>> {
    let raw_path = entry.path()?.to_path_buf();
    let Some(rel) = safe_rel_path(&raw_path) else {
        bail!("refusing to extract suspicious tar path {raw_path:?}");
    };
    let file_name = rel
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    if file_name.starts_with(".wh.") {
        if entry.header().entry_type() != tar::EntryType::Regular || entry.header().size()? != 0 {
            bail!("OCI whiteout {rel:?} must be an empty regular file");
        }
        let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        return Ok(Some(Whiteout { parent, file_name }));
    }

    // All ancestors of the entry must be real directories: earlier layers
    // may have planted symlinks that would otherwise resolve on the host.
    if let Some(parent) = rel.parent() {
        ensure_no_symlink_ancestors(tree, parent)?;
    }
    // A symlink at the entry's own path is replaced, never written through
    // (GNU tar replace semantics: unlink the link first).
    let target = tree.join(&rel);
    if let Ok(meta) = std::fs::symlink_metadata(&target)
        && meta.is_symlink()
    {
        remove_path(&target);
    }

    // Do not let an image inject host OverlayFS control attributes (redirect,
    // metacopy, whiteout, etc.). Only OCI whiteouts may create those markers.
    let mut xattrs = Vec::new();
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions {
            let extension = extension?;
            if let Some(name) = extension.key_bytes().strip_prefix(b"SCHILY.xattr.") {
                if name.starts_with(b"user.overlay.")
                    || !(name.starts_with(b"user.")
                        || name == b"security.capability"
                        || name == b"security.selinux"
                        || name == b"system.posix_acl_access"
                        || name == b"system.posix_acl_default")
                {
                    bail!("unsupported OCI xattr {:?}", String::from_utf8_lossy(name));
                }
                xattrs.push((name.to_vec(), extension.value_bytes().to_vec()));
            }
        }
    }
    match entry.header().entry_type() {
        tar::EntryType::Directory => {
            // tar::unpack_in intentionally ignores headers for '.'; explicit
            // restoration also avoids host ACL inheritance depending on order.
            let metadata = DirectoryMetadata {
                relative: rel.clone(),
                uid: entry.header().uid()?.try_into()?,
                gid: entry.header().gid()?.try_into()?,
                mode: entry.header().mode()?,
                mtime: entry.header().mtime()?,
                xattrs,
            };
            std::fs::create_dir_all(&target)?;
            if let Some(previous) = directories.iter_mut().find(|d| d.relative == rel) {
                *previous = metadata;
            } else {
                directories.push(metadata);
            }
            return Ok(None);
        }
        tar::EntryType::Regular | tar::EntryType::Symlink | tar::EntryType::Link => {
            if entry.header().entry_type() == tar::EntryType::Link {
                let link = entry
                    .link_name()?
                    .ok_or_else(|| eyre!("hard link has no target"))?;
                let relative_target = safe_rel_path(&link)
                    .ok_or_else(|| eyre!("unsafe OCI hard-link target {link:?}"))?;
                ensure_no_symlink_ancestors(tree, &relative_target)?;
                if !tree.join(&relative_target).exists() {
                    bail!(
                        "OCI hard-link target {} is absent from this layer; cross-layer and forward hard links are not supported by digest-only layer materialization",
                        relative_target.display()
                    );
                }
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // unpack_in() extracts at the entry's (already validated) path
            // and handles contents, permissions, symlinks and hard links.
            entry
                .unpack_in(tree)
                .wrap_err_with(|| format!("extract {}", rel.display()))?;
        }
        other
            if rel.starts_with("dev")
                && matches!(
                    other,
                    tar::EntryType::Char | tar::EntryType::Block | tar::EntryType::Fifo
                ) =>
        {
            // The guest kernel's CONFIG_DEVTMPFS_MOUNT supplies /dev. Device
            // nodes/fifos elsewhere are not silently dropped from the root.
            warn!(entry = %rel.display(), ?other, "ignoring device node from image /dev (guest devtmpfs replaces it)");
            // Nothing was created, so there is nothing to apply xattrs to.
            return Ok(None);
        }
        other => bail!(
            "unsupported OCI tar entry type {other:?} at {}; refusing to alter image semantics",
            rel.display()
        ),
    }
    apply_xattrs(&target, &xattrs)?;
    Ok(None)
}

pub(super) fn extract_entries<R: Read>(
    mut archive: tar::Archive<R>,
    tree: &Path,
    max_entries: u64,
) -> Result<(u64, Vec<PathBuf>, Vec<DirectoryReset>)> {
    let mut whiteouts = Vec::new();
    let mut directories = Vec::new();
    let mut count = 0_u64;
    for entry in archive.entries().wrap_err("iterate tar entries")? {
        count = count
            .checked_add(1)
            .ok_or_else(|| eyre!("OCI layer entry count overflows"))?;
        if count > max_entries {
            bail!("OCI layer exceeds the configured {max_entries} entry limit");
        }
        if let Some(whiteout) = unpack_entry(tree, entry.wrap_err("tar entry")?, &mut directories)?
        {
            whiteouts.push(whiteout);
        }
    }
    let opaque_dirs = whiteouts
        .iter()
        .filter(|w| w.file_name == ".wh..wh..opq")
        .map(|w| w.parent.clone())
        .collect::<std::collections::HashSet<_>>();
    let resets = whiteouts
        .iter()
        .map(|w| {
            if w.file_name == ".wh..wh..opq" {
                DirectoryReset {
                    path: w.parent.clone(),
                    descendants_only: true,
                }
            } else {
                DirectoryReset {
                    path: w.parent.join(w.file_name.strip_prefix(".wh.").unwrap()),
                    descendants_only: false,
                }
            }
        })
        .collect();
    for whiteout in whiteouts {
        if whiteout.file_name != ".wh..wh..opq" {
            if opaque_dirs.contains(&whiteout.parent) {
                continue;
            }
            let target_name = whiteout
                .file_name
                .strip_prefix(".wh.")
                .unwrap_or(&whiteout.file_name);
            let target = tree.join(&whiteout.parent).join(target_name);
            // OCI applies whiteouts before adding this layer. A same-layer
            // file therefore takes precedence regardless of tar entry order.
            if std::fs::symlink_metadata(&target).is_ok() {
                continue;
            }
        }
        apply_whiteout(tree, &whiteout.parent, &whiteout.file_name)?;
    }
    restore_directory_metadata(tree, &mut directories)?;
    Ok((
        count,
        directories.into_iter().map(|d| d.relative).collect(),
        resets,
    ))
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct DirectoryReset {
    pub path: PathBuf,
    pub descendants_only: bool,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct LayerStats {
    pub uncompressed_bytes: u64,
    pub entry_count: u64,
    pub diff_id: String,
    pub composefs_image_bytes: u64,
    pub object_store_bytes: u64,
    /// Only these directories were changed explicitly by the OCI changeset.
    pub explicit_directories: Vec<PathBuf>,
    pub directory_resets: Vec<DirectoryReset>,
}

pub(super) struct LimitedReader<R> {
    pub inner: R,
    pub limit: u64,
    pub bytes: u64,
    pub hasher: sha2::Sha256,
}

impl<R> LimitedReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            bytes: 0,
            hasher: sha2::Sha256::new(),
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let remaining = self.limit.saturating_sub(self.bytes);
        if remaining == 0 {
            let mut probe = [0_u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(std::io::Error::other(
                    "OCI layer exceeds configured uncompressed-byte limit",
                )),
            };
        }
        let allowed = buf
            .len()
            .min(usize::try_from(remaining.saturating_add(1)).unwrap_or(usize::MAX));
        let count = self.inner.read(&mut buf[..allowed])?;
        if count as u64 > remaining {
            return Err(std::io::Error::other(
                "OCI layer exceeds configured uncompressed-byte limit",
            ));
        }
        self.bytes = self.bytes.saturating_add(count as u64);
        self.hasher.update(&buf[..count]);
        Ok(count)
    }
}

pub(super) fn unpack_tar<R: Read>(
    reader: R,
    tree: &Path,
    max_bytes: u64,
    max_entries: u64,
) -> Result<LayerStats> {
    let mut limited = LimitedReader::new(reader, max_bytes);
    let mut archive = tar::Archive::new(&mut limited);
    archive.set_preserve_ownerships(true);
    archive.set_preserve_permissions(true);
    let (entry_count, explicit_directories, directory_resets) =
        extract_entries(archive, tree, max_entries)?;
    // Drain trailing tar padding and any bytes after the end marker so DiffID
    // and the byte bound cover the entire decompressed stream, not only entries.
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if limited.read(&mut buffer)? == 0 {
            break;
        }
    }
    Ok(LayerStats {
        uncompressed_bytes: limited.bytes,
        entry_count,
        diff_id: format!("sha256:{}", to_hex(&limited.hasher.finalize())),
        composefs_image_bytes: 0,
        object_store_bytes: 0,
        explicit_directories,
        directory_resets,
    })
}

/// Unpack a layer whose compressed digest was fully checked by `hash_file`.
/// The bounded decompressor also computes the config's uncompressed `DiffID`.
pub(super) fn unpack_layer<R: Read>(
    reader: R,
    compression: Compression,
    tree: &Path,
    max_bytes: u64,
    max_entries: u64,
) -> Result<LayerStats> {
    match compression {
        Compression::None => unpack_tar(reader, tree, max_bytes, max_entries),
        Compression::Gzip => unpack_tar(
            flate2::read::GzDecoder::new(reader),
            tree,
            max_bytes,
            max_entries,
        ),
        Compression::Zstd => unpack_tar(
            zstd::stream::read::Decoder::new(reader).wrap_err("zstd decoder init")?,
            tree,
            max_bytes,
            max_entries,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ch_driver::containers::test_support::{
        TempDir, append, append_entry, header, mount, tar_bytes, umount,
    };
    use std::os::unix::fs::MetadataExt;
    use tar::EntryType::{Char, Directory, Fifo, Link, Regular};

    fn unpack(bytes: &[u8], tree: &Path) -> Result<LayerStats> {
        unpack_layer(bytes, Compression::None, tree, 1024 * 1024, 10)
    }

    #[test]
    fn unpack_preserves_oci_metadata_and_rejects_overlay_xattrs() {
        let base = TempDir::new("metadata");
        // A valid v2 Linux capability xattr (CAP_NET_BIND_SERVICE).
        let capability = [1, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let bytes = tar_bytes(|tar| {
            tar.append_pax_extensions([(
                "SCHILY.xattr.security.capability",
                capability.as_slice(),
            )])
            .unwrap();
            append_entry(tar, "app", Regular, 0o4755, 1234, 2345, b"data");
            append_entry(tar, "tmp", Directory, 0o1777, 1234, 2345, b"");
        });
        unpack(&bytes, &base).unwrap();
        let meta = std::fs::metadata(base.join("app")).unwrap();
        assert_eq!(
            (meta.uid(), meta.gid(), meta.mode() & 0o7777),
            (1234, 2345, 0o4755)
        );
        assert_eq!(
            std::fs::metadata(base.join("tmp")).unwrap().mode() & 0o7777,
            0o1777
        );
        assert_eq!(
            xattr::get(base.join("app"), "security.capability").unwrap(),
            Some(capability.to_vec())
        );

        let bytes = tar_bytes(|tar| {
            tar.append_pax_extensions([(
                "SCHILY.xattr.trusted.overlay.redirect",
                b"/etc".as_slice(),
            )])
            .unwrap();
            append_entry(tar, "hostile", Regular, 0o1777, 1234, 2345, b"");
        });
        let _err = unpack(&bytes, &base).unwrap_err();
    }

    #[test]
    fn root_metadata_and_default_acls_are_order_independent() {
        // Linux ACL xattr v2: owner rwx, named uid 1234 rwx, group r-x,
        // mask rwx, other r-x. Children without an ACL must not inherit it.
        let mut acl = 2_u32.to_le_bytes().to_vec();
        for (tag, permission, id) in [
            (1_u16, 7_u16, u32::MAX),
            (2, 7, 1234),
            (4, 5, u32::MAX),
            (16, 7, u32::MAX),
            (32, 5, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(permission.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        for reversed in [false, true] {
            let base = TempDir::new("root-acl");
            let child = |tar: &mut tar::Builder<&mut Vec<u8>>| {
                append_entry(tar, "child", Regular, 0o755, 1234, 2345, b"");
            };
            let bytes = tar_bytes(|tar| {
                if reversed {
                    child(tar);
                }
                tar.append_pax_extensions([(
                    "SCHILY.xattr.system.posix_acl_default",
                    acl.as_slice(),
                )])
                .unwrap();
                let mut root = header(Directory, 0o1775, 1234, 2345);
                root.set_mtime(42);
                append(tar, root, "./", b"");
                if !reversed {
                    child(tar);
                }
            });
            unpack(&bytes, &base).unwrap();
            let meta = std::fs::metadata(&base).unwrap();
            assert_eq!(
                (meta.uid(), meta.gid(), meta.mode() & 0o7777, meta.mtime()),
                (1234, 2345, 0o1775, 42)
            );
            assert_eq!(
                xattr::get(base.join("child"), "system.posix_acl_access").unwrap(),
                None
            );
        }
    }

    #[test]
    fn unpacking_through_a_planted_symlink_is_refused() {
        let tree = TempDir::new("symlink");
        // Layer 1 plants a symlink; layer 2 must not resolve through it.
        std::os::unix::fs::symlink("/etc", tree.join("etc")).unwrap();

        assert!(ensure_no_symlink_ancestors(&tree, Path::new("etc/passwd")).is_err());
        assert!(ensure_no_symlink_ancestors(&tree, Path::new("etc")).is_err());
        ensure_no_symlink_ancestors(&tree, Path::new("usr/bin")).unwrap();
        // Whiteouts through the planted symlink are refused too.
        assert!(apply_whiteout(&tree, Path::new("etc"), ".wh..wh..opk").is_err());
    }

    #[test]
    fn maps_layer_media_types_to_compression() {
        for (media_type, compression) in [
            (
                "application/vnd.oci.image.layer.v1.tar+gzip",
                Some(Compression::Gzip),
            ),
            (
                "application/vnd.oci.image.layer.v1.tar+zstd",
                Some(Compression::Zstd),
            ),
            (
                "application/vnd.oci.image.layer.v1.tar",
                Some(Compression::None),
            ),
            (
                "application/vnd.docker.image.rootfs.diff.tar.gzip",
                Some(Compression::Gzip),
            ),
            ("application/vnd.oci.image.index", None),
        ] {
            assert_eq!(
                compression_of_media_type(media_type),
                compression,
                "{media_type}"
            );
        }
    }

    #[test]
    fn rejects_non_dev_special_tar_entries_and_skips_dev_nodes() {
        let tree = TempDir::new("special-tar");
        let fifo = tar_bytes(|tar| append_entry(tar, "run/host-socket", Fifo, 0o600, 0, 0, b""));
        let _err = unpack(&fifo, &tree).unwrap_err();

        let devices = tar_bytes(|tar| {
            for (path, minor) in [("dev/zero", 5), ("dev/null", 3)] {
                if path == "dev/null" {
                    // Labels on ignored /dev nodes must not be applied to a
                    // path that was never created.
                    tar.append_pax_extensions([(
                        "SCHILY.xattr.security.selinux",
                        b"system_u:object_r:null_device_t:s0".as_slice(),
                    )])
                    .unwrap();
                }
                let mut device = header(Char, 0o666, 0, 0);
                device.set_device_major(1).unwrap();
                device.set_device_minor(minor).unwrap();
                append(tar, device, path, b"");
            }
        });
        unpack(&devices, &tree).unwrap();
        for path in ["dev/zero", "dev/null"] {
            assert!(
                !tree.join(path).exists(),
                "guest devtmpfs supplies /dev nodes"
            );
        }
    }

    #[test]
    fn rejects_tar_path_traversal() {
        assert!(safe_rel_path(Path::new("etc/motd")).is_some());
        assert!(safe_rel_path(Path::new("/etc/motd")).is_some());
        assert!(safe_rel_path(Path::new("./etc/motd")).is_some());
        assert!(safe_rel_path(Path::new("./")).is_some());
        assert!(safe_rel_path(Path::new("../escape")).is_none());
        assert!(safe_rel_path(Path::new("a/../../escape")).is_none());
    }

    #[test]
    fn tar_layer_extraction_preserves_whiteouts_and_directory_metadata() {
        let base = TempDir::new("layer-tar");
        let bytes = tar_bytes(|tar| {
            append_entry(tar, "restricted/", Directory, 0o700, 0, 0, b"");
            append_entry(tar, "restricted/opaque/", Directory, 0o755, 0, 0, b"");
            for marker in [
                "restricted/.wh.removed",
                "restricted/.wh.gone",
                "restricted/opaque/.wh..wh..opq",
            ] {
                append_entry(tar, marker, Regular, 0o000, 0, 0, b"");
            }
            append_entry(tar, "restricted/removed", Regular, 0o644, 0, 0, b"same");
        });
        let tree = base.join("layer");
        std::fs::create_dir_all(&tree).unwrap();
        let stats = unpack(&bytes, &tree).unwrap();
        assert_eq!(stats.entry_count, 6);
        assert!(stats.uncompressed_bytes <= 1024 * 1024);
        for (name, max_bytes, max_entries) in
            [("oversized", 1024, 10), ("too-many", 1024 * 1024, 2)]
        {
            let limited = base.join(name);
            std::fs::create_dir_all(&limited).unwrap();
            let _err = unpack_layer(
                bytes.as_slice(),
                Compression::None,
                &limited,
                max_bytes,
                max_entries,
            )
            .unwrap_err();
        }
        assert_eq!(
            std::fs::metadata(tree.join("restricted")).unwrap().mode() & 0o777,
            0o700
        );
        // Same-layer content wins over a whiteout, independent of tar order.
        assert_eq!(
            std::fs::read(tree.join("restricted/removed")).unwrap(),
            b"same"
        );
        let whiteout = tree.join("restricted/gone");
        assert!(std::fs::metadata(&whiteout).unwrap().is_file());
        assert_eq!(std::fs::metadata(&whiteout).unwrap().len(), 0);
        assert!(
            xattr::get(&whiteout, "trusted.overlay.whiteout")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            xattr::get(tree.join("restricted/opaque"), "trusted.overlay.opaque").unwrap(),
            Some(b"y".to_vec())
        );
    }

    #[test]
    fn whiteouts_are_preserved_as_overlay_markers_in_layer_tree() {
        let tree = TempDir::new("whiteout");
        std::fs::create_dir_all(tree.join("bin")).unwrap();
        std::fs::write(tree.join("bin/cat"), b"preserved upper entry").unwrap();
        std::fs::write(tree.join("keep"), b"keep").unwrap();

        apply_whiteout(&tree, Path::new("bin"), ".wh.ls").unwrap();
        let whiteout = tree.join("bin/ls");
        assert!(std::fs::metadata(&whiteout).unwrap().is_file());
        assert_eq!(std::fs::metadata(&whiteout).unwrap().len(), 0);
        assert!(
            xattr::get(&whiteout, "trusted.overlay.whiteout")
                .unwrap()
                .is_some()
        );
        assert!(tree.join("bin/cat").exists());

        apply_whiteout(&tree, Path::new("bin"), ".wh..wh..opq").unwrap();
        assert_eq!(
            xattr::get(tree.join("bin"), "trusted.overlay.opaque").unwrap(),
            Some(b"y".to_vec())
        );
        assert!(tree.join("keep").exists());
    }

    #[test]
    fn hard_links_preserve_same_layer_content_and_reject_missing_or_escaping_targets() {
        for target in ["file", "base-only", "../escape"] {
            let base = TempDir::new("hardlink");
            let bytes = tar_bytes(|tar| {
                append_entry(tar, "file", Regular, 0o640, 1234, 2345, b"data");
                let mut link = header(Link, 0o640, 1234, 2345);
                link.set_link_name(target).unwrap();
                append(tar, link, "alias", b"");
            });
            let result = unpack(&bytes, &base);
            if target == "file" {
                result.unwrap();
                assert_eq!(std::fs::read(base.join("alias")).unwrap(), b"data");
                assert_eq!(
                    std::fs::metadata(base.join("file")).unwrap().ino(),
                    std::fs::metadata(base.join("alias")).unwrap().ino()
                );
            } else {
                let _err = result.unwrap_err();
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires CAP_SYS_ADMIN to mount OverlayFS"]
    async fn overlay_lower_order_and_whiteout_visibility() {
        let base = TempDir::under(Path::new("/var/lib/odorobo-overlay-test"), "overlay");
        let lower = base.join("base-layer");
        let upper_layer = base.join("top-layer");
        let merged = base.join("merged");
        std::fs::create_dir_all(lower.join("etc/opaque")).unwrap();
        std::fs::create_dir_all(upper_layer.join("etc/opaque")).unwrap();
        std::fs::create_dir_all(&merged).unwrap();
        std::fs::write(lower.join("etc/deleted"), b"old").unwrap();
        std::fs::write(lower.join("etc/replaced"), b"old").unwrap();
        std::fs::write(lower.join("etc/opaque/old"), b"old").unwrap();
        std::fs::write(upper_layer.join("etc/replaced"), b"top").unwrap();
        apply_whiteout(&upper_layer, Path::new("etc"), ".wh.deleted").unwrap();
        apply_whiteout(&upper_layer, Path::new("etc/opaque"), ".wh..wh..opq").unwrap();
        std::fs::write(upper_layer.join("etc/opaque/new"), b"new").unwrap();
        let options = format!("lowerdir={}:{},ro", upper_layer.display(), lower.display());
        mount("overlay", Some(options), &merged).await;
        let result = (|| {
            assert!(!merged.join("etc/deleted").exists());
            assert_eq!(std::fs::read(merged.join("etc/replaced"))?, b"top");
            assert!(!merged.join("etc/opaque/old").exists());
            assert_eq!(std::fs::read(merged.join("etc/opaque/new"))?, b"new");
            Ok::<(), std::io::Error>(())
        })();
        umount(&merged).await;
        result.unwrap();
    }
}
