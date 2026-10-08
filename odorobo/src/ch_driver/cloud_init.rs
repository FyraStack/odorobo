//! `NoCloud` seed-image generation for Cloud Hypervisor guests.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};
use stable_eyre::{
    Result,
    eyre::{WrapErr, eyre},
};

use crate::manifest::CloudInit;

const SEED_IMAGE_NAME: &str = "cloud-init.img";
const MIN_IMAGE_SIZE: u64 = 4 * 1024 * 1024;
const MAX_IMAGE_SIZE: u64 = 64 * 1024 * 1024;
const SECTOR_SIZE: u64 = 512;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

/// Create a read-only-attachable FAT seed disk recognized by `NoCloud` as CIDATA.
///
/// The image is stored inside the VM runtime directory, which is removed by the
/// VM teardown path. Its host-side mode is private because user/vendor-data may
/// contain credentials.
pub fn create_seed_image(runtime_dir: &Path, cloud_init: &CloudInit) -> Result<PathBuf> {
    let user_data = cloud_init
        .user_data
        .as_deref()
        .ok_or_else(|| eyre!("cloud-init user-data is required"))?;
    let meta_data = cloud_init
        .meta_data
        .as_deref()
        .ok_or_else(|| eyre!("cloud-init meta-data is required"))?;

    let user_data_size = u64::try_from(user_data.len())?;
    let meta_data_size = u64::try_from(meta_data.len())?;
    let vendor_data_size = cloud_init
        .vendor_data
        .as_deref()
        .map_or(Ok(0), |data| u64::try_from(data.len()))?;
    let content_size = user_data_size
        .checked_add(meta_data_size)
        .and_then(|size| size.checked_add(vendor_data_size))
        .ok_or_else(|| eyre!("cloud-init seed data size overflow"))?;
    let image_size = content_size
        .checked_add(MIN_IMAGE_SIZE)
        .ok_or_else(|| eyre!("cloud-init seed image size overflow"))?
        .max(MIN_IMAGE_SIZE)
        .div_ceil(SECTOR_SIZE)
        .checked_mul(SECTOR_SIZE)
        .ok_or_else(|| eyre!("cloud-init seed image size overflow"))?;
    if image_size > MAX_IMAGE_SIZE {
        return Err(eyre!(
            "cloud-init seed data exceeds the {} MiB image limit",
            MAX_IMAGE_SIZE / (1024 * 1024)
        ));
    }

    fs::create_dir_all(runtime_dir).wrap_err("failed to create VM runtime directory")?;
    let suffix = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temp_path = runtime_dir.join(format!(
        "{SEED_IMAGE_NAME}.tmp-{}-{suffix}",
        std::process::id()
    ));
    let seed_path = runtime_dir.join(SEED_IMAGE_NAME);

    let create_result = write_seed_image(&temp_path, image_size, cloud_init);
    if let Err(error) = create_result {
        drop(fs::remove_file(&temp_path));
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp_path, &seed_path) {
        drop(fs::remove_file(&temp_path));
        return Err(error).wrap_err("failed to publish cloud-init seed image");
    }
    Ok(seed_path)
}

fn write_seed_image(path: &Path, image_size: u64, cloud_init: &CloudInit) -> Result<()> {
    let mut image = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .wrap_err("failed to create cloud-init seed image")?;
    image
        .set_len(image_size)
        .wrap_err("failed to size cloud-init seed image")?;
    fatfs::format_volume(
        &mut image,
        FormatVolumeOptions::new().volume_label(*b"CIDATA     "),
    )
    .wrap_err("failed to format cloud-init seed image")?;
    drop(image);

    let image = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .wrap_err("failed to open formatted cloud-init seed image")?;
    let filesystem = FileSystem::new(image, FsOptions::new())
        .wrap_err("failed to mount cloud-init seed image")?;
    {
        let root = filesystem.root_dir();
        write_seed_file(&root, "user-data", cloud_init.user_data.as_deref())?;
        write_seed_file(&root, "meta-data", cloud_init.meta_data.as_deref())?;
        write_seed_file(&root, "vendor-data", cloud_init.vendor_data.as_deref())?;
    };
    filesystem
        .unmount()
        .wrap_err("failed to flush cloud-init seed image")?;

    let image = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .wrap_err("failed to sync cloud-init seed image")?;
    image
        .sync_all()
        .wrap_err("failed to sync cloud-init seed image")?;
    Ok(())
}

fn write_seed_file(
    root: &fatfs::Dir<'_, std::fs::File>,
    name: &str,
    contents: Option<&str>,
) -> Result<()> {
    let Some(contents) = contents else {
        return Ok(());
    };
    let mut file = root
        .create_file(name)
        .wrap_err_with(|| format!("failed to create NoCloud {name}"))?;
    file.write_all(contents.as_bytes())
        .wrap_err_with(|| format!("failed to write NoCloud {name}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{io::Read, path::PathBuf};

    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("odorobo-cloud-init-{}", ulid::Ulid::generate()))
    }

    #[test]
    fn creates_cidata_fat_seed_with_required_and_optional_files() {
        let runtime_dir = temp_dir();
        let cloud_init = CloudInit {
            user_data: Some("#cloud-config\nusers: []\n".to_owned()),
            meta_data: Some("instance-id: vm-1\n".to_owned()),
            vendor_data: Some("#cloud-config\npackages: [curl]\n".to_owned()),
        };

        let path = create_seed_image(&runtime_dir, &cloud_init).expect("seed image created");
        let image = std::fs::File::open(&path).expect("image opens");
        let filesystem = FileSystem::new(image, FsOptions::new()).expect("FAT image mounts");
        assert_eq!(filesystem.volume_label(), "CIDATA");
        let root = filesystem.root_dir();
        for (name, expected) in [
            ("user-data", cloud_init.user_data.as_deref().unwrap()),
            ("meta-data", cloud_init.meta_data.as_deref().unwrap()),
            ("vendor-data", cloud_init.vendor_data.as_deref().unwrap()),
        ] {
            let mut file = root.open_file(name).expect("seed file exists");
            let mut actual = String::new();
            file.read_to_string(&mut actual).expect("seed file reads");
            assert_eq!(actual, expected);
        }
        drop(root);
        filesystem.unmount().expect("image unmounts");
        std::fs::remove_dir_all(runtime_dir).expect("temporary image removed");
    }

    #[test]
    fn omits_vendor_data_when_not_supplied() {
        let runtime_dir = temp_dir();
        let cloud_init = CloudInit {
            user_data: Some("#cloud-config\n".to_owned()),
            meta_data: Some("instance-id: vm-2\n".to_owned()),
            vendor_data: None,
        };
        let path = create_seed_image(&runtime_dir, &cloud_init).expect("seed image created");
        let filesystem = FileSystem::new(
            std::fs::File::open(path).expect("image opens"),
            FsOptions::new(),
        )
        .expect("FAT image mounts");
        assert!(filesystem.root_dir().open_file("vendor-data").is_err());
        filesystem.unmount().expect("image unmounts");
        std::fs::remove_dir_all(runtime_dir).expect("temporary image removed");
    }

    #[test]
    fn rejects_seed_data_over_the_image_limit_before_creating_runtime_files() {
        let runtime_dir = temp_dir();
        let cloud_init = CloudInit {
            user_data: Some(
                "x".repeat(usize::try_from(MAX_IMAGE_SIZE + 1).expect("64 MiB fits usize")),
            ),
            meta_data: Some("instance-id: oversized\n".to_owned()),
            vendor_data: None,
        };

        let error = create_seed_image(&runtime_dir, &cloud_init)
            .expect_err("oversized seed data is rejected");
        assert!(error.to_string().contains("64 MiB"));
        assert!(!runtime_dir.exists());
    }
}
