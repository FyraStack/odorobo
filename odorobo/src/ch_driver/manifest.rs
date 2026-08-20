//! Cloud Hypervisor-specific conversion of the Odorobo VM manifest.
//!
//! The manifest is intentionally provider-neutral. This module is the only
//! place where the Cloud Hypervisor `VmConfig` representation is assembled
//! from manifest intent.

use cloud_hypervisor_client::models::{
    CpusConfig, DiskConfig, ImageType, MemoryConfig, PayloadConfig, PlatformConfig, VmConfig,
};
use stable_eyre::{Result, eyre::eyre};

use crate::manifest::{Storage, VmManifest};

/// Convert a validated Odorobo manifest to a Cloud Hypervisor configuration.
///
/// Node-local storage and networking are deliberately left to the existing
/// transform pipeline. This conversion only handles deterministic provider
/// fields and rejects manifest fields that cannot yet be represented safely.
pub fn to_vm_config(manifest: &VmManifest) -> Result<VmConfig> {
    manifest.validate()?;

    let desired = &manifest.desired;
    let disks = desired
        .storage
        .iter()
        .map(storage_to_disk)
        .collect::<Result<Vec<_>>>()?;

    if desired.cloud_init.is_some() {
        return Err(eyre!(
            "Cloud Hypervisor cloud-init conversion is not implemented yet"
        ));
    }
    if desired.vsock.is_some() {
        return Err(eyre!(
            "Cloud Hypervisor vsock conversion is not implemented yet"
        ));
    }
    if !desired.networks.is_empty() {
        return Err(eyre!(
            "Cloud Hypervisor network conversion must be supplied by the networking transform"
        ));
    }

    Ok(VmConfig {
        cpus: Some(CpusConfig {
            boot_vcpus: i32::try_from(desired.compute.vcpus)
                .map_err(|_| eyre!("vCPU count exceeds Cloud Hypervisor limits"))?,
            max_vcpus: i32::try_from(desired.compute.max_vcpus.unwrap_or(desired.compute.vcpus))
                .map_err(|_| eyre!("maximum vCPU count exceeds Cloud Hypervisor limits"))?,
            ..Default::default()
        }),
        memory: Some(MemoryConfig {
            size: i64::try_from(desired.compute.memory_bytes)
                .map_err(|_| eyre!("memory size exceeds Cloud Hypervisor limits"))?,
            ..Default::default()
        }),
        payload: PayloadConfig {
            firmware: desired
                .boot
                .firmware
                .clone()
                .or_else(|| Some("/var/lib/odorobo/CLOUDHV.fd".to_owned())),
            kernel: desired.boot.kernel.clone(),
            cmdline: desired.boot.cmdline.clone(),
            ..Default::default()
        },
        disks: (!disks.is_empty()).then_some(disks),
        platform: Some(PlatformConfig {
            serial_number: Some("ds=nocloud".to_owned()),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn storage_to_disk(storage: &Storage) -> Result<DiskConfig> {
    let path = match (&storage.uri, storage.volume_id) {
        (Some(uri), None) if uri.starts_with("file://") => uri
            .strip_prefix("file://")
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| eyre!("storage {} has an invalid file URI", storage.id))?,
        (Some(uri), None) => {
            return Err(eyre!(
                "storage {} URI scheme is not supported by the Cloud Hypervisor file converter: {uri}",
                storage.id
            ));
        }
        (None, Some(volume_id)) => {
            return Err(eyre!(
                "storage {} references volume {volume_id}; volume resolution belongs to the storage transform",
                storage.id
            ));
        }
        _ => return Err(eyre!("storage {} has no usable source", storage.id)),
    };

    Ok(DiskConfig {
        path: Some(path),
        image_type: Some(ImageType::Raw),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Boot, Compute, DesiredState, Metadata};
    use ulid::Ulid;

    fn minimal() -> VmManifest {
        VmManifest {
            api_version: crate::manifest::MANIFEST_VERSION,
            id: Ulid::generate(),
            desired: DesiredState {
                metadata: Metadata {
                    name: "test".to_owned(),
                    ..Default::default()
                },
                compute: Compute {
                    vcpus: 2,
                    memory_bytes: 1024,
                    ..Default::default()
                },
                boot: Boot::default(),
                ..Default::default()
            },
            observed: None,
        }
    }

    #[test]
    fn converts_compute_and_defaults() {
        let config = to_vm_config(&minimal()).expect("minimal manifest converts");
        assert_eq!(config.cpus.expect("cpus").boot_vcpus, 2);
        assert_eq!(config.memory.expect("memory").size, 1024);
        assert_eq!(
            config.platform.expect("platform").serial_number.as_deref(),
            Some("ds=nocloud")
        );
    }

    #[test]
    fn rejects_networks_until_transform_contract_is_connected() {
        let mut manifest = minimal();
        manifest.desired.networks.push(crate::manifest::Network {
            id: "net://private".to_owned(),
            ..Default::default()
        });
        assert!(to_vm_config(&manifest).is_err());
    }
}
