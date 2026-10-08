# Guest provisioning and devices

## Cloud-init / NoCloud

For `desired.cloud_init`, Odorobo writes the supplied `user_data`, `meta_data`,
and optional `vendor_data` files verbatim to a VFAT seed disk labelled
`CIDATA`. Cloud Hypervisor receives that artifact as a read-only raw disk
following the manifest's boot disks. It is created under the VM runtime
directory and removed with that directory when the VM actor is stopped or
deleted. The implementation follows the same NoCloud seed contract used by
[`pi-vm`](https://github.com/TheHeroBrine422/pi-vm/blob/main/src/vm.rs) and its
[`bake` command](https://github.com/TheHeroBrine422/pi-vm/blob/main/src/commands/bake.rs):
the guest receives a read-only `CIDATA` device with `meta-data` and
`user-data` (and, here, optional `vendor-data`). `pi-vm` currently packages
those files as an ISO9660 image; Odorobo uses VFAT, another NoCloud-supported
seed format.

The guest image must include a working `cloud-init` installation with the
NoCloud datasource enabled, and its kernel must support the virtual block
device. Odorobo does not install cloud-init in the guest, rewrite or validate
the supplied YAML, generate a separate `network-config`, or provide a metadata
HTTP service. In particular, NoCloud metadata should include a stable,
non-empty `instance-id`. cloud-init normally applies user-data on first boot;
changing the manifest does not reset cloud-init's per-instance state. The
driver does not depend on firmware serial-number/DMI fields, so using this
seed disk does not require EDK2 or a particular firmware boot mode. A guest
image without cloud-init/NoCloud support will simply not apply the seed.

Cloud-init data may contain secrets. The seed image is created with private
host permissions and attached read-only, but guest root can read it and host
administrators can access the VM runtime directory. Keep credentials out of
manifests and logs where possible; prefer short-lived credentials.

## Vsock

`desired.vsock` creates a Cloud Hypervisor virtio-vsock device. The guest image
needs a kernel with `VSOCKETS` and `VIRTIO_VSOCKETS` support and a guest-side
service that uses `AF_VSOCK`; Odorobo does not install or run a guest agent.
The Cloud Hypervisor `socket` value is a **host-side Unix socket used by
Cloud Hypervisor to proxy vsock connections**, not a guest filesystem path or
a guest TCP port. A host integration must connect to that socket and implement
the service protocol expected by the guest. The commonly used host CID is 2;
guest CID 0–2 and `u32::MAX` are reserved, and Odorobo allocates guest CIDs
from 3 through 4294967294.

When `guest_cid` is omitted, the agent allocates the first free CID and stores
the assignment in a locked, atomically updated node-local registry. The
default registry is `/var/lib/odorobo/vsock-cids.json`; set
`ODOROBO_VSOCK_CID_REGISTRY` to change it. Successful assignments survive
agent/VM shutdown and restart, and are released on explicit VM deletion;
reservations from failed startup attempts are rolled back. A requested CID is
reserved if free; a conflict fails VM creation rather than silently assigning
a different CID. `GetVMInfo` returns the effective VM manifest, including the
allocated CID in `observed.vsock_guest_cid` and the desired socket path;
automatic allocation leaves `desired.vsock.guest_cid` omitted.

## Runtime and migration limits

The seed builder caps images at 64 MiB, but this is not the distributed API
payload limit: actor requests are limited to 1 MiB (including the serialized
manifest), and HTTP JSON bodies to 2 MiB. Keep snippets well below 1 MiB.

Live vsock migration must provide the source guest CID; the destination reserves
that exact CID or rejects a collision. It must never allocate a substitute for
the device CID restored by the migration stream.

Cloud Hypervisor's receive API restores source-side device paths and currently
provides no destination-config override. Node-specific disk mappings and runtime
paths must therefore match across hosts; Odorobo's preparation transforms alone
do not rewrite the incoming stream. Live migration of cloud-init-enabled VMs is
rejected until a verified destination seed-path contract is implemented. This
restriction does not apply to importing resolved provisioning snippets into a
new VM.

## Proxmox migration mapping

The manifest is not a Proxmox compatibility format, and this repository does
not currently contain a Proxmox importer. A migration tool should translate
the *resolved Proxmox cloud-init inputs*, not copy a generated cloud-init drive
or opaque QEMU arguments:

| Proxmox input | Odorobo target / migration behavior |
| --- | --- |
| `cicustom` user snippet | `desired.cloud_init.user_data` (content, not the Proxmox storage reference) |
| `cicustom` meta snippet | `desired.cloud_init.meta_data`; preserve or deliberately regenerate `instance-id` |
| `cicustom` vendor snippet | `desired.cloud_init.vendor_data` |
| Proxmox-generated user, SSH-key, DNS, and other cloud-init settings | Compose into `user_data`/`vendor_data` using cloud-init's documented format; do not copy Proxmox-specific keys verbatim |
| `ipconfigN` / Proxmox network-config drive | Not represented by a separate manifest field yet. The importer must reject/report this as unsupported or explicitly translate it into a guest-supported mechanism; do not silently drop static addressing |
| Proxmox `args` or custom QEMU vsock arguments | Not automatically translated. Set `desired.vsock` explicitly and confirm the guest/host protocol and socket endpoint |

Password fields and rendered cloud-init snippets can contain secrets. A
migration/import flow must avoid logging them and must not place plaintext
passwords in manifests unless a separately reviewed secret-handling contract
allows it. Mapping of Proxmox network configuration and secret references is
future migration work, not implicit behavior of the Cloud Hypervisor driver.
