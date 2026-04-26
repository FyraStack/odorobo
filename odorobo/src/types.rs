use std::collections::BTreeMap;

use aide::OperationIo;
use bytesize::ByteSize;
use cloud_hypervisor_client::models::VmConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

mod bytesize_as_u64 {
    use bytesize::ByteSize;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(size: &ByteSize, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(size.as_u64())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ByteSize, D::Error> {
        Ok(ByteSize(u64::deserialize(d)?))
    }
}

mod opt_bytesize_as_u64 {
    use bytesize::ByteSize;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(size: &Option<ByteSize>, s: S) -> Result<S::Ok, S::Error> {
        match size {
            Some(b) => s.serialize_some(&b.as_u64()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ByteSize>, D::Error> {
        Ok(Option::<u64>::deserialize(d)?.map(ByteSize))
    }
}

// Newtype so aide can generate a path parameter schema for Ulid.
/// VM ID, in the format of ULID
#[repr(transparent)]
#[derive(Serialize, Deserialize, Debug, JsonSchema, OperationIo, Default, Clone)]
pub struct VmId(#[schemars(with = "String")] pub Ulid);

/// Volume ID, in the format of ULID
#[repr(transparent)]
#[derive(Serialize, Deserialize, Debug, JsonSchema, OperationIo, Default, Clone)]
pub struct VolumeId(#[schemars(with = "String")] pub Ulid);

/// A URI pointing to the volume's location, e.g an iSCSI URL in `iscsi-inq` format, a local file, or an RBD image.
///
/// examples:
/// - `iscsi://[<username>[%<password>]@]<host>[:<port>]/<target-iqn-name>/<lun>`
/// - `file:///path/to/volume.img`
/// - `rbd://<pool>/<image>`
#[repr(transparent)]
#[derive(Serialize, Deserialize, Debug, JsonSchema, OperationIo, Clone)]
pub struct StorageUri(#[schemars(with = "String")] pub url::Url);

impl Default for StorageUri {
    fn default() -> Self {
        StorageUri(url::Url::parse("file:///tmp").unwrap())
    }
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Default, Clone)]
pub struct CreateVMRequest {
    /// Data of the VM to create
    pub data: VMData,
    /// Whether to boot the VM immediately after creation
    pub boot: bool,
}


/// An internal, debug-only request for creating a VM.
///
/// please don't use this in production, this is for debugging
///
/// PUT /vms/
#[derive(Serialize, Deserialize, Debug, OperationIo, Default, Clone)]
pub struct DebugCreateVMRequest {
    /// Data of the VM to create
    pub vm_config: VmConfig,
    /// Whether to boot the VM immediately after creation
    pub boot: bool,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Default, Clone)]
pub struct VMData {
    /// VM ID. This is a ULID string.
    #[schemars(with = "String")]
    pub id: Ulid,
    /// Name of the VM.
    pub name: String,
    /// Number of vCPUs allocated to the VM.
    pub vcpus: u32,
    /// Optional maximum number of vCPUs the VM can scale up to, if supported by the underlying hypervisor.
    pub max_vcpus: Option<u32>,
    /// Amount of RAM in bytes allocated to the VM.
    #[schemars(with = "u64")]
    #[serde(with = "bytesize_as_u64")]
    pub memory: ByteSize,
    /// Image used for the VM.
    pub image: String,
    /// List of volumes to attach to the VM.
    #[serde(default)]
    pub volumes: Vec<Volume>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Default)]
pub struct UpdateVMRequest {
    /// Updated name of the VM.
    pub name: Option<String>,
    /// Updated number of vCPUs allocated to the VM.
    pub vcpus: Option<u32>,
    /// Updated maximum number of vCPUs the VM can scale up to, if supported by the underlying hypervisor.
    pub max_vcpus: Option<u32>,
    /// Updated amount of RAM in bytes allocated to the VM.
    #[schemars(with = "Option<u64>")]
    #[serde(with = "opt_bytesize_as_u64")]
    pub memory: Option<ByteSize>,
    /// Updated list of volumes to attach to the VM. This will replace the existing list of attached volumes.
    #[serde(default)]
    pub volumes: Vec<Volume>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Default)]
pub enum VMStatus {
    /// VM is currently running and operational.
    Running,
    /// VM is currently shut down, not running.
    Stopped,
    /// VM is being provisioned, being set up and started.
    #[default]
    Provisioning,
    Error(String), // error message
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Default)]
pub struct ObjectMetadata {
    /// Labels associated with the object.
    pub labels: BTreeMap<String, String>,
    /// Annotations associated with the object.
    pub annotations: BTreeMap<String, String>,
}

/// Detailed information about a running VM
// probably move this somewhere else
#[derive(Serialize, Deserialize, Debug, JsonSchema, Default)]
pub struct VirtualMachine {
    /// VM configuration
    pub data: VMData,

    /// Currently scheduled node for the VM,
    /// if any.
    ///
    /// None means the VM is not currently scheduled to any node
    /// (e.g. VM is shut down, underlying volume still provisioning, compute unschedulable, etc.)
    pub node: Option<String>,
    /// Current status of the VM
    pub status: VMStatus,

    /// Metadata
    pub metadata: Option<ObjectMetadata>,

    // placement stuff....
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Default)]
pub struct VMListResponse {
    /// List of VMs currently known by the agent.
    pub vms: Vec<VmId>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Default, Clone)]
pub struct Volume {
    /// Volume ID. This is a ULID string.
    #[schemars(with = "String")]
    pub id: Ulid,
    /// Name of the volume.
    pub name: String,
    /// Size of the volume in bytes.
    #[schemars(with = "u64")]
    #[serde(with = "bytesize_as_u64")]
    pub size: ByteSize,

    /// A URI pointing to the volume's location, e.g an iSCSI URL in `iscsi-inq` format, a local file, or an RBD image.
    ///
    /// examples:
    /// - `iscsi://[<username>[%<password>]@]<host>[:<port>]/<target-iqn-name>/<lun>`
    /// - `file:///path/to/volume.img`
    /// - `rbd://<pool>/<image>`
    ///
    pub uri: StorageUri,
}
// for now
pub type CreateVolumeRequest = Volume;

#[derive(Serialize, Deserialize, Debug, JsonSchema, Default)]
pub enum VolumeStatus {
    /// Available in the pool, not yet attached to any VM
    Available,
    /// Volume is currently attached to a VM,
    /// This may affect scheduling by preferring a node
    /// where this volume is already attached to, if possible.
    Attached(String),

    /// Volume is being provisioned, being carved
    /// from the pool.
    #[default]
    Provisioning,
    Error(String), // error message
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Default)]
pub struct VolumeInfo {
    pub data: Volume,
    pub status: VolumeStatus,
}

/// A compute node in the cluster. This is used for scheduling VMs to nodes.
#[derive(Serialize, Deserialize, Debug, JsonSchema, Default)]
pub struct Node {
    /// Hostname or identifier of the node.
    pub hostname: String,
    /// Total number of vCPUs available on the node.
    pub total_vcpus: u32,
    /// Total amount of RAM in bytes available on the node.
    #[schemars(with = "u64")]
    #[serde(with = "bytesize_as_u64")]
    pub total_memory: ByteSize,
    #[serde(default)]
    pub status: NodeStatus,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Default)]
pub struct NodeStatus {
    /// CPU usage
    pub cpu_usage: f32,
}
