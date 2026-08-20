//! Stable Odorobo VM intent manifest.
//!
//! This module deliberately does not mirror Cloud Hypervisor's `VmConfig`.
//! The manifest is the provider-neutral desired state exchanged with Odorobo;
//! provider-specific configuration is produced by the driver.

use std::{collections::BTreeMap, path::Path};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::Error as DeError};
use thiserror::Error;
use ulid::Ulid;

/// Version of the provider-neutral manifest contract.
///
/// This is intentionally independent from the Cloud Hypervisor API version. A
/// driver may translate one manifest version into a different provider API
/// shape, while callers continue to exchange the same Odorobo-level intent.
pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest version {0} is unsupported")]
    UnsupportedVersion(u32),
    #[error("metadata must define a non-empty name")]
    EmptyMetadataName,
    #[error("manifest must define at least one vCPU")]
    NoVcpus,
    #[error("max_vcpus ({max}) must be greater than or equal to vcpus ({vcpus})")]
    MaxVcpusLessThanBoot { vcpus: u32, max: u32 },
    #[error("memory must be greater than zero")]
    NoMemory,
    #[error("storage attachment must define a non-empty id")]
    EmptyStorageId,
    #[error(
        "storage attachment {0} must define exactly one usable source (URI or volume reference)"
    )]
    InvalidStorageSource(String),
    #[error("storage attachment {0} has a duplicate id")]
    DuplicateStorageId(String),
    #[error("storage attachment {0} cannot be both a boot attachment and read-only")]
    ReadOnlyBootStorage(String),
    #[error("manifest cannot define more than one boot storage attachment")]
    MultipleBootStorageAttachments,
    #[error("network {0} must define an id")]
    NetworkWithoutId(usize),
    #[error("cloud-init user-data and meta-data must be supplied together")]
    IncompleteCloudInit,
    #[error("cloud-init user-data and meta-data must both be non-empty")]
    EmptyCloudInitData,
    #[error("vsock requires a non-zero guest CID")]
    InvalidVsockCid,
    #[error("vsock requires a non-empty socket path")]
    InvalidVsockSocket,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VmManifest {
    /// Version of this manifest's serialized contract, not the hypervisor API.
    pub api_version: u32,
    /// Stable identity used to correlate desired state with runtime reports.
    #[schemars(with = "String")]
    pub id: Ulid,
    /// Control-plane intent that Odorobo should reconcile onto a node.
    pub desired: DesiredState,
    /// Runtime information reported by Odorobo; it must not be treated as input
    /// when calculating the next desired provider configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<ObservedState>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeserializableVmManifest {
    api_version: u32,
    id: Ulid,
    desired: DesiredState,
    #[serde(default)]
    observed: Option<ObservedState>,
}

impl<'de> Deserialize<'de> for VmManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let manifest = DeserializableVmManifest::deserialize(deserializer)?;
        let manifest = Self {
            api_version: manifest.api_version,
            id: manifest.id,
            desired: manifest.desired,
            observed: manifest.observed,
        };
        manifest.validate().map_err(D::Error::custom)?;
        Ok(manifest)
    }
}

impl VmManifest {
    /// Checks contract-level invariants before a provider-specific conversion.
    ///
    /// Keeping this check before Cloud Hypervisor translation prevents invalid
    /// intent from being hidden by defaults or rejected later as an opaque
    /// provider error.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.api_version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion(self.api_version));
        }
        self.desired.validate()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DesiredState {
    pub metadata: Metadata,
    pub compute: Compute,
    #[serde(default)]
    pub storage: Vec<Storage>,
    #[serde(default)]
    pub networks: Vec<Network>,
    #[serde(default)]
    pub placement: Placement,
    pub boot: Boot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_init: Option<CloudInit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vsock: Option<Vsock>,
}

impl DesiredState {
    /// Validates relationships between fields that JSON deserialization alone
    /// cannot express, such as a boot storage attachment needing a writable source.
    fn validate(&self) -> Result<(), ManifestError> {
        if self.metadata.name.trim().is_empty() {
            return Err(ManifestError::EmptyMetadataName);
        }
        if self.compute.vcpus == 0 {
            return Err(ManifestError::NoVcpus);
        }
        if let Some(max) = self.compute.max_vcpus
            && max < self.compute.vcpus
        {
            return Err(ManifestError::MaxVcpusLessThanBoot {
                vcpus: self.compute.vcpus,
                max,
            });
        }
        if self.compute.memory_bytes == 0 {
            return Err(ManifestError::NoMemory);
        }
        // A disk source remains abstract here. URI resolution and volume
        // attachment belong to storage backends, so the manifest only checks
        // that the converter has one usable source from which to begin.
        let mut storage_ids = std::collections::HashSet::with_capacity(self.storage.len());
        let mut has_boot_storage = false;
        for storage in &self.storage {
            if storage.id.trim().is_empty() {
                return Err(ManifestError::EmptyStorageId);
            }
            if !storage_ids.insert(&storage.id) {
                return Err(ManifestError::DuplicateStorageId(storage.id.clone()));
            }
            if storage
                .uri
                .as_deref()
                .is_some_and(|uri| uri.trim().is_empty())
                || storage.uri.is_some() == storage.volume_id.is_some()
            {
                return Err(ManifestError::InvalidStorageSource(storage.id.clone()));
            }
            if storage.boot && storage.read_only {
                return Err(ManifestError::ReadOnlyBootStorage(storage.id.clone()));
            }
            if storage.boot {
                if has_boot_storage {
                    return Err(ManifestError::MultipleBootStorageAttachments);
                }
                has_boot_storage = true;
            }
        }
        for (index, network) in self.networks.iter().enumerate() {
            if network.id.trim().is_empty() {
                return Err(ManifestError::NetworkWithoutId(index));
            }
        }
        // NoCloud treats user-data and meta-data as one instance-data set.
        // Accepting only half of it would produce a provider configuration that
        // looks valid but can fail during guest initialization.
        if let Some(cloud) = &self.cloud_init {
            if cloud.user_data.is_some() != cloud.meta_data.is_some()
                || (cloud.user_data.is_none() && cloud.meta_data.is_none())
            {
                return Err(ManifestError::IncompleteCloudInit);
            }
            if cloud
                .user_data
                .as_deref()
                .is_some_and(|data| data.trim().is_empty())
                || cloud
                    .meta_data
                    .as_deref()
                    .is_some_and(|data| data.trim().is_empty())
            {
                return Err(ManifestError::EmptyCloudInitData);
            }
        }
        if let Some(vsock) = &self.vsock {
            if vsock.guest_cid == 0 {
                return Err(ManifestError::InvalidVsockCid);
            }
            if vsock.socket.trim().is_empty() || !Path::new(&vsock.socket).is_absolute() {
                return Err(ManifestError::InvalidVsockSocket);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Compute {
    pub vcpus: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vcpus: Option<u32>,
    pub memory_bytes: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub volume_id: Option<Ulid>,
    #[serde(default)]
    pub boot: bool,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default)]
    pub required_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub affinity: Vec<AffinityRule>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AffinityRule {
    pub strictness: AffinityStrictness,
    pub affinity_type: AffinityType,
    pub direction: AffinityDirection,
    pub requirements: Vec<AffinityRequirement>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AffinityStrictness {
    Required,
    Preferred { weight: i64 },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AffinityType {
    VirtualMachine,
    Agent,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AffinityDirection {
    Normal,
    Anti,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AffinityRequirement {
    pub key: String,
    pub table: MetadataTable,
    pub operator: Operator,
    pub values: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetadataTable {
    Label,
    Annotation,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    In,
    NotIn,
    Lt,
    Gt,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Boot {
    #[serde(default)]
    pub start: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CloudInit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta_data: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Vsock {
    pub guest_cid: u32,
    pub socket: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservedState {
    /// Odorobo's normalized view of the VM lifecycle.
    pub status: ObservedStatus,
    /// Node currently hosting the VM, which can differ from desired placement
    /// while scheduling or migration is in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Optional provider state retained for diagnostics, not reconciliation
    /// input. Consumers should not depend on this string's provider format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_hypervisor_state: Option<String>,
    /// Actionable failure detail when the observed status is `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservedStatus {
    #[default]
    Unknown,
    Pending,
    Running,
    Stopped,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_round_trips() {
        let fixtures = [
            include_str!("../../docs/fixtures/manifest/minimal.json"),
            include_str!("../../docs/fixtures/manifest/storage-backed.json"),
            include_str!("../../docs/fixtures/manifest/networked.json"),
            include_str!("../../docs/fixtures/manifest/cloud-init.json"),
            include_str!("../../docs/fixtures/manifest/vsock.json"),
        ];

        for json in fixtures {
            let manifest: VmManifest = serde_json::from_str(json).expect("valid fixture");
            manifest.validate().expect("valid manifest");
            let encoded = serde_json::to_string(&manifest).expect("serializable manifest");
            let decoded: VmManifest = serde_json::from_str(&encoded).expect("round trip");
            assert_eq!(manifest, decoded);
        }
    }

    #[test]
    fn rejects_unknown_fields() {
        let json = r#"{
            "api_version": 1,
            "id": "01J00000000000000000000000",
            "desired": {
                "metadata": { "name": "vm" },
                "compute": { "vcpus": 1, "memory_bytes": 1 },
                "boot": { "start": false },
                "unexpected": true
            }
        }"#;
        serde_json::from_str::<VmManifest>(json).unwrap_err();
    }

    fn minimal_manifest() -> VmManifest {
        serde_json::from_str(include_str!("../../docs/fixtures/manifest/minimal.json"))
            .expect("valid minimal fixture")
    }

    #[test]
    fn rejects_unsupported_version_during_validation_and_deserialization() {
        let mut manifest = minimal_manifest();
        manifest.api_version = MANIFEST_VERSION + 1;
        assert_eq!(
            manifest.validate(),
            Err(ManifestError::UnsupportedVersion(MANIFEST_VERSION + 1))
        );

        let json = include_str!("../../docs/fixtures/manifest/minimal.json")
            .replace("\"api_version\": 1", "\"api_version\": 2");
        serde_json::from_str::<VmManifest>(&json).expect_err("unsupported version must reject");
    }

    #[test]
    fn generated_schema_describes_manifest_shape() {
        let schema = schemars::schema_for!(VmManifest);
        let schema = serde_json::to_value(schema).expect("schema is serializable");
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("manifest schema has properties");

        for field in ["api_version", "id", "desired", "observed"] {
            assert!(
                properties.contains_key(field),
                "missing schema field {field}"
            );
        }
        assert_eq!(properties["id"]["type"], "string");
    }

    #[test]
    fn rejects_invalid_compute() {
        let mut manifest = minimal_manifest();
        manifest.desired.compute.max_vcpus = Some(0);
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::MaxVcpusLessThanBoot { .. })
        ));

        let invalid_json = include_str!("../../docs/fixtures/manifest/minimal.json")
            .replace("\"vcpus\": 1", "\"vcpus\": 0");
        serde_json::from_str::<VmManifest>(&invalid_json).unwrap_err();
    }

    #[test]
    fn rejects_invalid_storage() {
        let mut manifest = minimal_manifest();
        manifest.desired.storage.push(Storage {
            id: "invalid".to_owned(),
            uri: Some("file:///disk.img".to_owned()),
            volume_id: Some(Ulid::generate()),
            ..Default::default()
        });
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::InvalidStorageSource(_))
        ));

        manifest.desired.storage.clear();
        manifest.desired.storage.push(Storage {
            id: "root".to_owned(),
            uri: Some("  ".to_owned()),
            ..Default::default()
        });
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::InvalidStorageSource(_))
        ));

        manifest.desired.storage[0].uri = Some("file:///disk.img".to_owned());
        manifest.desired.storage.push(Storage {
            id: "root".to_owned(),
            uri: Some("file:///other.img".to_owned()),
            ..Default::default()
        });
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::DuplicateStorageId(_))
        ));

        manifest.desired.storage.clear();
        manifest.desired.storage.push(Storage {
            id: "  ".to_owned(),
            uri: Some("file:///disk.img".to_owned()),
            ..Default::default()
        });
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::EmptyStorageId)
        ));

        manifest.desired.storage.clear();
        manifest.desired.storage.push(Storage {
            id: "root".to_owned(),
            uri: Some("file:///disk.img".to_owned()),
            boot: true,
            ..Default::default()
        });
        manifest.desired.storage.push(Storage {
            id: "other".to_owned(),
            uri: Some("file:///other.img".to_owned()),
            boot: true,
            ..Default::default()
        });
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::MultipleBootStorageAttachments)
        ));

        manifest.desired.storage.clear();
        manifest.desired.storage.push(Storage {
            id: "root".to_owned(),
            uri: Some("file:///disk.img".to_owned()),
            boot: true,
            read_only: true,
            ..Default::default()
        });
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::ReadOnlyBootStorage(_))
        ));
    }

    #[test]
    fn rejects_invalid_vsock_and_metadata() {
        let mut manifest = minimal_manifest();
        manifest.desired.vsock = Some(Vsock {
            guest_cid: 42,
            socket: "relative.sock".to_owned(),
        });
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::InvalidVsockSocket)
        ));

        manifest.desired.vsock = None;
        manifest.desired.metadata.name = "  ".to_owned();
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::EmptyMetadataName)
        ));
    }

    #[test]
    fn rejects_invalid_network_and_cloud_init() {
        let mut manifest = minimal_manifest();
        manifest.desired.networks.push(Network {
            id: "  ".to_owned(),
            ..Default::default()
        });
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::NetworkWithoutId(0))
        ));

        manifest.desired.networks.clear();
        manifest.desired.cloud_init = Some(CloudInit::default());
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::IncompleteCloudInit)
        ));

        manifest.desired.cloud_init = Some(CloudInit {
            user_data: Some("  ".to_owned()),
            meta_data: Some("instance-id: vm".to_owned()),
        });
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::EmptyCloudInitData)
        ));
    }

    #[test]
    fn serializes_affinity() {
        let mut manifest = minimal_manifest();
        manifest.desired.placement.affinity.push(AffinityRule {
            strictness: AffinityStrictness::Preferred { weight: 10 },
            affinity_type: AffinityType::VirtualMachine,
            direction: AffinityDirection::Anti,
            requirements: vec![AffinityRequirement {
                key: "tier".to_owned(),
                table: MetadataTable::Label,
                operator: Operator::In,
                values: vec!["batch".to_owned()],
            }],
        });
        let encoded = serde_json::to_string(&manifest).expect("affinity is serializable");
        assert!(encoded.contains("preferred"));
        assert!(encoded.contains("virtual_machine"));
        assert!(encoded.contains("anti"));
    }
}
