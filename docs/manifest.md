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

The current Cloud Hypervisor conversion in `odorobo/src/ch_driver/actor.rs`
consumes vCPUs, maximum vCPUs, memory, and the image as a disk. Firmware and
serial/platform defaults are currently driver-owned. Network, volume-to-disk,
cloud-init, and vsock conversion remain provider integration work; their
manifest fields are defined here so those later conversions have a stable
contract and explicit ownership boundary.

## State ownership

`desired` is supplied by the control plane and is the source of truth for what
Odorobo should provision. It contains:

- `metadata`: stable name, labels, and annotations.
- `compute`: boot vCPUs, optional scaling ceiling, and memory in bytes.
- `disks`: ordered disk intent. A disk references either a storage URI or a
  provisioned volume ID; `boot` identifies the boot disk.
- `networks`: stable network IDs and optional guest MAC addresses.
- `placement`: scheduling hints, not a host binding unless `node` is set by the
  scheduler.
- `boot`: whether to start after provisioning and optional firmware/kernel/
  command-line intent.
- `cloud_init`: paired NoCloud user-data and meta-data.
- `vsock`: guest CID and the desired host-side socket location.

`observed` is reported by Odorobo and is never used as desired input. It records
status, the node currently running the VM, the provider's runtime state, and an
error message when applicable. Cloud Hypervisor configuration and generated
paths are observed/driver-owned implementation details, not manifest fields.

## Validation and evolution

A manifest must use a supported `api_version`, have a non-empty metadata name,
non-zero vCPUs and memory, and satisfy these relationships:

- `max_vcpus` must be at least `vcpus`.
- Every disk must have exactly one usable source (URI or volume reference), and
  a boot disk cannot be read-only. At most one disk may be marked as boot.
- Every network must have a non-empty, non-whitespace ID.
- Cloud-init must provide non-empty configuration with user-data and meta-data
  supplied together.
- A vsock guest CID must be non-zero and its socket must be an absolute path.

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

For example:

```json
{
  "api_version": 1,
  "id": "01J00000000000000000000005",
  "desired": {
    "metadata": { "name": "vm", "labels": {}, "annotations": {} },
    "compute": { "vcpus": 2, "memory_bytes": 2147483648 },
    "disks": [], "networks": [], "placement": {},
    "boot": { "start": true },
    "vsock": { "guest_cid": 42, "socket": "/run/odorobo/vms/vm/vsock.sock" }
  }
}
```
