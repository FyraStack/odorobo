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
reservations from failed startup attempts are rolled back only after confirmed
process exit. An active lease cannot be reused—even for the same VM ID—until
exit is confirmed. Every reservation has a distinct persisted lease token;
startup rollback, exit recording, and deletion retries use that token so one
incarnation cannot release a newer incarnation's lease.

On SIGTERM/SIGINT the agent stops its VMs before exiting (waiting up to 60
seconds), so each VMM is reaped and its lease is marked stopped. A second
signal exits immediately. The shipped systemd unit uses `KillMode=mixed` so
only the agent gets the SIGTERM (the VMMs run in its cgroup), and a 90 second
`TimeoutStopSec` so systemd doesn't SIGKILL it mid-shutdown. If you write your
own unit, keep both. The single-instance lockfile (`/var/lock/odorobo.lock`) is
an flock, so an agent that was killed outright doesn't block the next start.

The registry also records each lease's VMM process by PID, kernel start time,
and boot ID, so a process from before a reboot is never mistaken for a new one.
After an unclean agent exit, a restarted agent treats a lease as stopped once
that exact process is gone, meaning every one of its threads has exited. An explicit delete
of a VM whose VMM is still running with no actor left (for example after the
agent was killed) SIGKILLs that exact process before releasing its lease and
runtime files. A lease that never had its process recorded (the agent died
between reserving the CID and spawning the VMM, or recording failed) still
needs manual recovery. A requested CID is
reserved if free; a conflict fails VM creation rather than silently assigning
a different CID. `GetVMInfo` returns the effective VM manifest, including the
allocated CID in `observed.vsock_guest_cid` and the desired socket path;
automatic allocation leaves `desired.vsock.guest_cid` omitted.

The scheduler tracks which node must clean up a VM and its CID lease by libp2p
peer ID. Each node therefore keeps a persistent identity key at
`/var/lib/odorobo/peer-key` (`--peer-key-path` or `ODOROBO_PEER_KEY_PATH`), so
a restarted agent can still confirm deletes for VMs it owned. Instances sharing
a host need distinct key paths.

## Runtime and migration limits

Deletion is acknowledged only after the known VM-owning agents confirm process
exit and node-local cleanup. Missing owners keep deletion pending; retry after
those agents become reachable, or delete with `?force=true` to drop owners whose
agent can't be reached at all (an agent that answers with an error, or that
might have gotten the request but timed out, still fails the delete). Every
create and delete the scheduler sends carries a time-ordered stamp, so a delete
that arrives after its VM ID was recreated is acked without touching the new
VM. Cleanup ownership survives discovery-cache eviction in scheduler
memory, and after a scheduler restart it is rebuilt from agent status: each
agent reports running VMs and `retained_vms` (IDs that still hold node-local
state, like a stopped CID lease). An agent that was unreachable since the
restart can't be known as an owner, so keep node identities stable when
recovering failed agents.

If an agent reports a VM the scheduler already finished deleting (like a node
skipped by a forced delete coming back), the scheduler deletes it there in the
background. Stopped VMs are never cleaned up this way. Until that node acks a
delete or reports a full status without the VM, the ID can't be recreated, since
the old copy could still be running on the same storage. Use a new VM ID if the
node isn't coming back. Stop/delete tombstones are kept in scheduler memory
only: stopped ones for an hour, deleted ones for a day (longer while a node
might still hold the VM), and pending deletes until they finish. A scheduler
restart forgets all of this.

Storage teardown releases only resources acquired by Odorobo. Shared mappings
are reference-counted within the agent process; pre-existing external mappings
are borrowed rather than detached. Failed releases retain process-local cleanup
state and block ID reuse until an explicit delete retries successfully. This
state does not survive an unclean agent restart; inspect and recover leftover
host mappings manually rather than assuming they can safely be detached. A
failed acquisition command cannot prove ownership of a mapping that appeared
concurrently. Such ambiguous resources are quarantined and never automatically
detached; they require manual reconciliation rather than a destructive retry.
There is no automatic in-process reset for uncertain ownership: stop managed
VMs and reconcile affected mappings/sessions with external users before
restarting the agent to clear that process-local state. Restart alone is not
proof that the old VMM exited or that storage can safely be detached.

Storage operations run as agent-owned tasks with bounded caller waits. A timeout
or cancelled request does not abandon an in-flight map/logout: cleanup keeps
its ownership record, and the same resource cannot be reused while release is
pending. Unrelated resources are not serialized behind backend I/O. Ambiguous
release outcomes also require manual reconciliation; an apparently absent device
is not sufficient if a timed-out daemon operation might still complete later.

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
