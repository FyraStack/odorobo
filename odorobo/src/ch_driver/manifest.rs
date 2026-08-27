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

use crate::manifest::{Storage, VmManifest};

/// Convert a validated Odorobo manifest to a Cloud Hypervisor configuration.
///
/// This is the provider boundary: callers provide only Odorobo intent, while
/// this module chooses Cloud Hypervisor defaults and emits logical storage and
/// network references for the node-local transform pipeline to resolve.
/// Fields without a defined Cloud Hypervisor transport are rejected rather than
/// silently omitted.
pub fn to_vm_config(manifest: &VmManifest) -> Result<VmConfig> {
    manifest.validate()?;

    let desired = &manifest.desired;
    // Keep URI-backed disks logical until StorageDriverTransformer resolves them
    // to node-local paths. This preserves enough source identity for teardown.
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

    if desired.cloud_init.is_some() {
        return Err(eyre!(
            "Cloud Hypervisor cloud-init conversion is not implemented yet"
        ));
    }
    // Unlike cloud-init, vsock has a direct Cloud Hypervisor representation, so
    // the declarative manifest fields can be passed through without a sidecar.
    let vsock = desired.vsock.as_ref().map(|vsock| VsockConfig {
        cid: i64::from(vsock.guest_cid),
        socket: vsock.socket.clone(),
        id: Some("odorobo-vsock".to_owned()),
        ..Default::default()
    });

    Ok(VmConfig {
        cpus: Some(CpusConfig {
            boot_vcpus: i32::try_from(desired.compute.vcpus)
                .map_err(|_| eyre!("vCPU count exceeds Cloud Hypervisor limits"))?,
            max_vcpus: i32::try_from(desired.compute.max_vcpus.unwrap_or(desired.compute.vcpus))
                .map_err(|_| eyre!("maximum vCPU count exceeds Cloud Hypervisor limits"))?,
            ..Default::default()
        }),
        memory: Some(MemoryConfig {
            size: i64::try_from(desired.compute.memory.as_u64())
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
    use bytesize::ByteSize;
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
                    memory: ByteSize::b(1024),
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
    fn converts_networks_for_the_transform_pipeline() {
        let mut manifest = minimal();
        manifest.desired.networks.push(crate::manifest::Network {
            id: "private".to_owned(),
            mac_address: Some("02:00:00:00:00:01".to_owned()),
        });
        let config = to_vm_config(&manifest).expect("network manifest converts");
        let network = &config.net.expect("network config")[0];
        assert_eq!(network.id.as_deref(), Some("net://private"));
        assert_eq!(network.mac.as_deref(), Some("02:00:00:00:00:01"));
    }

    #[test]
    fn converts_multiple_storage_attachments_for_storage_transforms() {
        let mut manifest = minimal();
        manifest.desired.storage = vec![
            Storage {
                id: "root".to_owned(),
                uri: Some("rbd://pool/root".to_owned()),
                boot: true,
                ..Default::default()
            },
            Storage {
                id: "data".to_owned(),
                uri: Some("file:///var/lib/data.img".to_owned()),
                read_only: true,
                ..Default::default()
            },
        ];
        let config = to_vm_config(&manifest).expect("storage manifest converts");
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
            guest_cid: 42,
            socket: "/run/odorobo/vsock.sock".to_owned(),
        });
        let config = to_vm_config(&manifest).expect("vsock manifest converts");
        let vsock = config.vsock.expect("vsock config");
        assert_eq!(vsock.cid, 42);
        assert_eq!(vsock.socket, "/run/odorobo/vsock.sock");
        assert_eq!(vsock.id.as_deref(), Some("odorobo-vsock"));
    }

    #[test]
    fn rejects_cloud_init_without_a_transport_contract() {
        let mut manifest = minimal();
        manifest.desired.cloud_init = Some(crate::manifest::CloudInit {
            user_data: Some("#cloud-config\n".to_owned()),
            meta_data: Some("instance-id: test\n".to_owned()),
        });
        let error = to_vm_config(&manifest).expect_err("cloud-init transport is not defined");
        assert!(error.to_string().contains("cloud-init"));
    }
}
