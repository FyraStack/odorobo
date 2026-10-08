//! Cloud Hypervisor-specific conversion of the Odorobo VM manifest.
//!
//! The manifest is intentionally provider-neutral. This module is the only
//! place where the Cloud Hypervisor `VmConfig` representation is assembled
//! from manifest intent.

use cloud_hypervisor_client::models::{
    CpusConfig, DiskConfig, ImageType, MemoryConfig, NetConfig, PayloadConfig, PlatformConfig,
    VmConfig, VsockConfig,
};
use stable_eyre::{Result, eyre::eyre};
use std::path::Path;

use crate::{
    ch_driver::cloud_init::create_seed_image,
    manifest::{Storage, VmManifest},
};

/// Convert a validated Odorobo manifest to a Cloud Hypervisor configuration.
///
/// This is the provider boundary: callers provide only Odorobo intent, while
/// this module chooses Cloud Hypervisor defaults and emits logical storage and
/// network references for the node-local transform pipeline to resolve.
/// Fields without a defined Cloud Hypervisor transport are rejected rather than
/// silently omitted.
pub fn to_vm_config(manifest: &VmManifest, runtime_dir: &Path) -> Result<VmConfig> {
    manifest.validate()?;

    let desired = &manifest.desired;
    // Keep URI-backed disks logical until StorageDriverTransformer resolves them
    // to node-local paths. This preserves enough source identity for teardown.
    // Cloud Hypervisor has no per-disk boot index; it uses the disks array
    // order when selecting a bootable device. Keep the manifest order intact.
    let disks = desired
        .storage
        .iter()
        .map(storage_to_disk)
        .collect::<Result<Vec<_>>>()?;
    // NetworkTransform recognizes these net:// IDs and assigns deterministic TAP
    // names without making the provider-neutral manifest host-specific.
    let networks = desired
        .networks
        .iter()
        .map(|network| NetConfig {
            id: Some(format!("net://{}", network.id)),
            mac: network.mac_address.clone(),
            ..Default::default()
        })
        .collect::<Vec<_>>();

    // Unlike cloud-init, vsock has a direct Cloud Hypervisor representation, so
    // the declarative manifest fields can be passed through after the node-local
    // allocator resolves an optional guest CID.
    let vsock = desired
        .vsock
        .as_ref()
        .map(|vsock| -> Result<VsockConfig> {
            let guest_cid = vsock
                .guest_cid
                .ok_or_else(|| eyre!("vsock guest CID was not allocated by the agent"))?;
            Ok(VsockConfig {
                cid: i64::from(guest_cid),
                socket: vsock.socket.clone(),
                id: Some("odorobo-vsock".to_owned()),
                ..Default::default()
            })
        })
        .transpose()?;

    let boot_vcpus = i32::try_from(desired.compute.vcpus)
        .map_err(|_| eyre!("vCPU count exceeds Cloud Hypervisor limits"))?;
    let max_vcpus = i32::try_from(desired.compute.max_vcpus.unwrap_or(desired.compute.vcpus))
        .map_err(|_| eyre!("maximum vCPU count exceeds Cloud Hypervisor limits"))?;
    let memory_size = i64::try_from(desired.compute.memory_bytes)
        .map_err(|_| eyre!("memory size exceeds Cloud Hypervisor limits"))?;

    // Create the artifact only after all other fallible manifest conversions
    // have succeeded, so rejected configs do not leave runtime files behind.
    let cloud_init_disk = desired
        .cloud_init
        .as_ref()
        .map(|cloud_init| create_seed_image(runtime_dir, cloud_init))
        .transpose()?;

    let mut disks = disks;
    if let Some(seed_path) = cloud_init_disk {
        disks.push(DiskConfig {
            id: Some("cloud-init".to_owned()),
            path: Some(seed_path.to_string_lossy().into_owned()),
            readonly: Some(true),
            image_type: Some(ImageType::Raw),
            ..Default::default()
        });
    }

    Ok(VmConfig {
        cpus: Some(CpusConfig {
            boot_vcpus,
            max_vcpus,
            ..Default::default()
        }),
        memory: Some(MemoryConfig {
            size: memory_size,
            ..Default::default()
        }),
        payload: PayloadConfig {
            firmware: desired.boot.firmware.clone().or_else(|| {
                // Only default to the firmware when doing a firmware boot
                // (no kernel specified). Direct kernel boot must not set a
                // firmware, or Cloud Hypervisor rejects the config with
                // "Specifying a kernel is not supported when a firmware is
                // provided".
                if desired.boot.kernel.is_some() {
                    None
                } else {
                    Some("/var/lib/odorobo/CLOUDHV.fd".to_owned())
                }
            }),
            kernel: desired.boot.kernel.clone(),
            cmdline: desired.boot.cmdline.clone(),
            ..Default::default()
        },
        disks: (!disks.is_empty()).then_some(disks),
        net: (!networks.is_empty()).then_some(networks),
        vsock,
        platform: Some(PlatformConfig {
            serial_number: Some("ds=nocloud".to_owned()),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Build a logical disk config while retaining the source URI for the storage
/// transform. Volume IDs are rejected until their resolution contract defines
/// how an agent obtains a device path.
fn storage_to_disk(storage: &Storage) -> Result<DiskConfig> {
    let path = match (&storage.uri, storage.volume_id) {
        (Some(uri), None)
            if uri.starts_with("file://")
                || uri.starts_with("rbd://")
                || uri.starts_with("iscsi://") =>
        {
            uri.clone()
        }
        (Some(uri), None) => {
            return Err(eyre!(
                "storage {} URI scheme is unsupported: {uri}",
                storage.id
            ));
        }
        (None, Some(volume_id)) => {
            return Err(eyre!(
                "storage {} references volume {volume_id}; volume resolution contract is not defined yet",
                storage.id
            ));
        }
        _ => return Err(eyre!("storage {} has no usable source", storage.id)),
    };

    Ok(DiskConfig {
        id: Some(storage.id.clone()),
        path: Some(path),
        readonly: Some(storage.read_only),
        image_type: Some(ImageType::Raw),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Boot, Compute, DesiredState, Metadata};
    use std::{path::PathBuf, sync::OnceLock};
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

    fn convert(manifest: &VmManifest) -> Result<VmConfig> {
        static TEST_RUNTIME: OnceLock<PathBuf> = OnceLock::new();
        let root = TEST_RUNTIME.get_or_init(|| {
            std::env::temp_dir().join(format!("odorobo-manifest-test-{}", Ulid::generate()))
        });
        to_vm_config(manifest, &root.join(manifest.id.to_string()))
    }

    #[test]
    fn converts_compute_and_defaults() {
        let config = convert(&minimal()).expect("minimal manifest converts");
        assert_eq!(config.cpus.expect("cpus").boot_vcpus, 2);
        assert_eq!(config.memory.expect("memory").size, 1024);
        assert_eq!(
            config.platform.expect("platform").serial_number.as_deref(),
            Some("ds=nocloud")
        );
    }

    #[test]
    fn firmware_default_only_for_firmware_boot() {
        // No kernel, no explicit firmware -> defaults to the firmware.
        let config = convert(&minimal()).expect("minimal manifest converts");
        assert_eq!(
            config.payload.firmware.as_deref(),
            Some("/var/lib/odorobo/CLOUDHV.fd")
        );

        // Kernel set, no explicit firmware -> no firmware (direct kernel boot).
        let mut manifest = minimal();
        manifest.desired.boot.kernel = Some("/tmp/vmlinuz".to_owned());
        let config = convert(&manifest).expect("kernel manifest converts");
        assert_eq!(config.payload.firmware, None);
        assert_eq!(config.payload.kernel.as_deref(), Some("/tmp/vmlinuz"));

        // Explicit firmware wins even when a kernel is also set.
        let mut manifest = minimal();
        manifest.desired.boot.kernel = Some("/tmp/vmlinuz".to_owned());
        manifest.desired.boot.firmware = Some("/custom/fw.fd".to_owned());
        let config = convert(&manifest).expect("explicit firmware manifest converts");
        assert_eq!(config.payload.firmware.as_deref(), Some("/custom/fw.fd"));
    }

    #[test]
    fn converts_networks_for_the_transform_pipeline() {
        let mut manifest = minimal();
        manifest.desired.networks.push(crate::manifest::Network {
            id: "private".to_owned(),
            mac_address: Some("02:00:00:00:00:01".to_owned()),
        });
        let config = convert(&manifest).expect("network manifest converts");
        let network = &config.net.expect("network config")[0];
        assert_eq!(network.id.as_deref(), Some("net://private"));
        assert_eq!(network.mac.as_deref(), Some("02:00:00:00:00:01"));
    }

    #[test]
    fn preserves_manifest_storage_order_for_cloud_hypervisor_boot() {
        let mut manifest = minimal();
        manifest.desired.storage = vec![
            Storage {
                id: "data".to_owned(),
                uri: Some("file:///var/lib/data.img".to_owned()),
                ..Default::default()
            },
            Storage {
                id: "root".to_owned(),
                uri: Some("file:///var/lib/root.img".to_owned()),
                ..Default::default()
            },
        ];

        let config = convert(&manifest).expect("ordered storage manifest converts");
        let disks = config.disks.as_ref().expect("disk configs");
        let serialized = serde_json::to_value(&config).expect("config serializes");

        assert_eq!(
            disks
                .iter()
                .filter_map(|disk| disk.id.as_deref())
                .collect::<Vec<_>>(),
            ["data", "root"]
        );
        assert_eq!(serialized["disks"][0]["id"].as_str(), Some("data"));
        assert_eq!(serialized["disks"][1]["id"].as_str(), Some("root"));
    }

    #[test]
    fn converts_multiple_storage_attachments_for_storage_transforms() {
        let mut manifest = minimal();
        manifest.desired.storage = vec![
            Storage {
                id: "root".to_owned(),
                uri: Some("rbd://pool/root".to_owned()),
                ..Default::default()
            },
            Storage {
                id: "data".to_owned(),
                uri: Some("file:///var/lib/data.img".to_owned()),
                read_only: true,
                ..Default::default()
            },
        ];
        let config = convert(&manifest).expect("storage manifest converts");
        let disks = config.disks.expect("disk configs");
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].id.as_deref(), Some("root"));
        assert_eq!(disks[0].path.as_deref(), Some("rbd://pool/root"));
        assert_eq!(disks[1].id.as_deref(), Some("data"));
        assert_eq!(disks[1].readonly, Some(true));
    }

    #[test]
    fn converts_vsock_configuration() {
        let mut manifest = minimal();
        manifest.desired.vsock = Some(crate::manifest::Vsock {
            guest_cid: Some(42),
            socket: "/run/odorobo/vsock.sock".to_owned(),
        });
        let config = convert(&manifest).expect("vsock manifest converts");
        let vsock = config.vsock.expect("vsock config");
        assert_eq!(vsock.cid, 42);
        assert_eq!(vsock.socket, "/run/odorobo/vsock.sock");
        assert_eq!(vsock.id.as_deref(), Some("odorobo-vsock"));
    }

    #[test]
    fn converts_cloud_init_to_read_only_nocloud_seed_disk() {
        let mut manifest = minimal();
        manifest.desired.cloud_init = Some(crate::manifest::CloudInit {
            user_data: Some("#cloud-config\n".to_owned()),
            meta_data: Some("instance-id: test\n".to_owned()),
            vendor_data: Some("#cloud-config\npackages: [curl]\n".to_owned()),
        });
        let config = convert(&manifest).expect("cloud-init manifest converts");
        let disks = config.disks.expect("seed disk config");
        let seed = disks
            .last()
            .expect("seed disk is appended after boot disks");
        assert_eq!(seed.id.as_deref(), Some("cloud-init"));
        assert_eq!(seed.readonly, Some(true));
        assert!(seed.path.as_deref().unwrap().ends_with("cloud-init.img"));
        assert_eq!(seed.image_type, Some(ImageType::Raw));
    }

    #[test]
    fn rejects_vsock_without_allocated_cid() {
        let mut manifest = minimal();
        manifest.desired.vsock = Some(crate::manifest::Vsock {
            guest_cid: None,
            socket: "/run/odorobo/vsock.sock".to_owned(),
        });
        let error = convert(&manifest).expect_err("unallocated CID must not reach CH");
        assert!(error.to_string().contains("not allocated"));
    }
}
