# Odorobo VM manifest contract

The Odorobo VM manifest is the provider-neutral description of VM intent. It is
not a Cloud Hypervisor `VmConfig`; the Cloud Hypervisor driver owns conversion,
node-local paths, and runtime details. The current contract is version `1` and
is represented by `odorobo::manifest::VmManifest`.

## Existing field inventory

The legacy `VirtualMachine` model in `odorobo/src/types.rs` currently combines
intent and runtime data: `VMData` contains identity, name, vCPU limits, memory,
an image, volumes, and network IDs, while `VirtualMachine` adds node, status,
metadata, and affinity. The manifest separates those concerns so the control
plane can provide stable intent without depending on the legacy shape.

The Cloud Hypervisor driver consumes compute and boot settings, resolves
network and storage references through node-local transforms, and translates
cloud-init into a NoCloud seed disk. Firmware and serial/platform defaults are
driver-owned. Vsock is passed to Cloud Hypervisor directly after the node-local
agent allocates or reserves its guest CID.

## State ownership

`desired` is supplied by the control plane and is the source of truth for what
Odorobo should provision. It contains:

- `metadata`: stable name, labels, and annotations.
- `compute`: boot vCPUs, optional scaling ceiling, and memory in bytes.
- `storage`: ordered storage attachments. An attachment references either a storage URI or a
  provisioned volume ID. The array order is the attachment order and preferred disk boot
  order: the first attachment is the preferred boot disk, followed by later attachments.
  Cloud Hypervisor has no per-disk boot index, so providers must preserve this order when
  translating the manifest. The control plane owns attachment identity and ordering; the
  provider driver/storage transform resolves the URI or volume ID to a node-local device path.
- `networks`: stable network IDs and optional guest MAC addresses. The control plane owns
  the attachment identity and requested MAC; the provider networking transform resolves
  the network to a host interface or tap device.
- `placement`: scheduling hints, including an optional node, required node labels,
  and affinity rules. Affinity rules support required or weighted-preferred
  VM/agent matching; `inverse` selects anti-affinity, with OR-ed
  label/annotation requirements.
- `boot`: whether to start after provisioning and optional firmware/kernel/
  command-line intent.
- `cloud_init`: paired NoCloud user-data and meta-data, plus optional
  vendor-data. The Cloud Hypervisor driver writes these files to a FAT seed
  image labelled `CIDATA` and attaches it as a read-only raw disk. The image is
  kept in the VM runtime directory and removed when the VM is deleted or shut
  down; keep credentials in these fields private accordingly.
- `vsock`: optional guest CID and the desired host-side socket location. When
  omitted, the node-local agent allocates a stable CID; explicitly requested
  CIDs are reserved and checked for conflicts on that node. An automatically
  assigned CID is reported under `observed.vsock_guest_cid`, while
  `desired.vsock.guest_cid` remains unset so another node can allocate its own
  local CID. See [guest provisioning](provisioning.md)
  for the guest kernel, host proxy socket, restart, and migration requirements.

`observed` is reported by Odorobo and is never used as desired input. It records
status, the node currently running the VM, the provider's runtime state, an
error message when applicable, and the effective node-local vsock guest CID.
Cloud Hypervisor configuration and generated paths are observed/driver-owned
implementation details, not manifest fields.

Providers may reject a valid manifest field when they cannot implement it, but
must report that explicitly. They must not silently discard storage, network,
boot, cloud-init, or vsock intent.

## Validation and evolution

A manifest must use a supported `api_version`, have a non-empty metadata name,
non-zero vCPUs and memory, and satisfy these relationships:

- `max_vcpus` must be at least `vcpus`.
- Every storage attachment must have a non-empty ID and exactly one usable source (URI or volume reference).
  Storage order must be preserved when attachments are translated to the provider.
- Affinity requirements within a rule are OR-ed; rules are combined according to
  their strictness, and `inverse` negates a rule's result. `lt` and `gt` comparisons require exactly one
  finite numeric value.
- Every network must have a non-empty, non-whitespace ID.
- Cloud-init must provide non-empty user-data and meta-data together. Optional
  vendor-data must not be empty. The seed-image builder caps images at 64 MiB;
  this is not an API payload allowance. Distributed actor requests are limited
  to 1 MiB and HTTP JSON bodies to 2 MiB, including the rest of the manifest
  and serialization overhead. Keep provisioning snippets well below 1 MiB.
- An explicitly supplied vsock guest CID must be between 3 and 4294967294
  (CIDs 0-2 and `u32::MAX` are reserved); an omitted CID is allocated by the
  node. The socket must be an absolute path.

Invalid field combinations are rejected during deserialization, as are unknown
fields, rather than silently interpreted. New fields should be added in a future manifest version when they
change semantics; unreleased formats do not require Proxmox compatibility
layers. Providers may reject a valid manifest field they cannot implement, with
a clear unsupported-field error, rather than dropping it. This contract is
therefore intentionally forward-evolving, not a compatibility layer for
Proxmox or unreleased Odorobo formats.

## Examples

Representative JSON fixtures are in [`fixtures/manifest`](fixtures/manifest):

- [`minimal.json`](fixtures/manifest/minimal.json)
- [`storage-backed.json`](fixtures/manifest/storage-backed.json)
- [`networked.json`](fixtures/manifest/networked.json)
- [`cloud-init.json`](fixtures/manifest/cloud-init.json)
- [`vsock.json`](fixtures/manifest/vsock.json)

For example, storage attachments are listed in preferred disk boot order. Here `root` is
presented before `data`:

```json
"storage": [
  { "id": "root", "uri": "rbd://vms/root" },
  { "id": "data", "volume_id": "01J00000000000000000000002" }
]
```

For example:

```json
{
  "api_version": 1,
  "id": "01J00000000000000000000005",
  "desired": {
    "metadata": { "name": "vm", "labels": {}, "annotations": {} },
    "compute": { "vcpus": 2, "memory_bytes": 2147483648 },
    "storage": [],
    "networks": [],
    "placement": {},
    "boot": { "start": true },
    "vsock": { "guest_cid": 42, "socket": "/run/odorobo/vms/vm/vsock.sock" }
  }
}
```
