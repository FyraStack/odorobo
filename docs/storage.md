# VM storage contract

Odorobo accepts storage attachments through the VM manifest's ordered
`desired.storage` list. The Cloud Hypervisor driver resolves each supported URI
to a host-local file or block-device path when creating a VM. It does not copy
an RBD image onto each compute node.

The initial storage contract supports local files and kernel-mapped Ceph RBD
images. iSCSI remains available through the existing transform, but is outside
the Ceph lifecycle contract described here. iSCSI URIs must be exactly
`iscsi://<portal>[:<port>]/<iqn>/<lun>` (port defaults to 3260), where `<lun>`
is plain decimal digits that fit in 32 bits, optionally written `lun<n>`. The
LUN is required. URIs with a malformed or out-of-range LUN, extra path segments,
a query or fragment, or a percent-encoded portal or IQN are rejected before any
login rather than falling back to LUN 0. This defines behavior for new
manifest attachments; it does not migrate existing Odorobo deployments or
rewrite existing local images. A manifest attachment has a stable
attachment ID, one source (`uri` or `volume_id`), and an optional `read_only`
flag. `volume_id` resolution is not yet defined; use a supported URI.

## Local file images

Use an absolute `file://` URI, for example:

```json
{ "id": "root", "uri": "file:///var/lib/odorobo/images/root.raw" }
```

The path refers to a file already present on the node running the VM. Odorobo
passes the path to Cloud Hypervisor; it does not copy, create, or delete the
image. The file must be readable (and writable unless the attachment is
read-only) by the service account and accessible inside the Cloud Hypervisor
process's mount namespace. Local-file attachments have no external resource
to unmap during teardown.

## Ceph RBD images

Specify an existing RBD image as `rbd://<pool>/<image>`:

```json
{
  "id": "root",
  "uri": "rbd://odorobo-vms/root-01",
  "read_only": false
}
```

The pool and image are part of the VM attachment URI, not a local cache path.
For the initial contract, pool and image names use ASCII letters, digits, `.`,
`_`, or `-`; names cannot be empty or start with `-`. Pool names must be
lowercase; the transformer validates the original URI and rejects case variants
rather than normalizing them. The URI must have exactly one image path segment.
Credentials, ports, query parameters, fragments, namespaces, and snapshot
selectors are not accepted. In particular,
`rbd://pool/ns/image` and `rbd://pool/image@snapshot` are not supported by this
contract. These restrictions prevent an ambiguous URI from selecting a
resource different from the one Odorobo tracks.

The referenced pool and image must already exist and be reachable by the
configured Ceph client. Odorobo maps the image with the `rbd` CLI and passes the
resulting kernel block device to Cloud Hypervisor. It does not create pools or
images, clone/import images, resize them, or delete them. Deleting a VM removes
its attachment and may unmap a mapping owned by Odorobo; it never deletes the
RBD image or pool. Provisioning, snapshotting, importing, and image deletion
remain operator-managed operations.

`read_only: true` makes the Cloud Hypervisor disk read-only. Odorobo's host-side
attachment reference counting prevents a mapping from being unmapped while
another VM still references it; it does not make concurrent writable use of a
filesystem safe. Share writable images only when the guest workload and
filesystem are explicitly designed for multi-writer access. Read-only sharing
is the usual safe shared-image pattern.

### Node prerequisites and Ceph client configuration

Each VM node needs the `rbd` executable (typically from `ceph-common`), the
kernel RBD module, permission to map block devices, and access to the Ceph
cluster. Odorobo passes these optional environment variables through to `rbd`:

| Environment variable | `rbd` option | Purpose |
| --- | --- | --- |
| `CEPH_CONFIG` | `--conf` | Ceph configuration file path |
| `CEPH_ID` | `--id` | Ceph client identity, such as `odorobo` |
| `CEPH_KEYFILE` | `--keyfile` | Path to the client key file; do not put key contents in a manifest |
| `CEPH_CLUSTER` | `--cluster` | Ceph cluster name |

When omitted, the Ceph CLI uses its normal defaults. These values are node
configuration: all RBD references on a node use the same configured client
context. Keep key files readable only by the Odorobo service account and grant
only the pool capabilities needed by the node. Pool and image selection is
configured per attachment in its `rbd://` URI.

Odorobo asks `rbd device map` not to wait on a udev event in its own namespace.
When the host's Ceph udev rule is present, the driver prefers the stable path
`/dev/rbd/<pool>/<image>`. Otherwise, it falls back to the device reported by
`rbd device list` (for example `/dev/rbd0`). The udev rule commonly provided by
Ceph packages is:

```udev
KERNEL=="rbd[0-9]*", ENV{DEVTYPE}=="disk", PROGRAM="/usr/bin/ceph-rbdnamer %k", SYMLINK+="rbd/%c"
KERNEL=="rbd[0-9]*", ENV{DEVTYPE}=="partition", PROGRAM="/usr/bin/ceph-rbdnamer %k", SYMLINK+="rbd/%c-part%n"
```

Install it as `/etc/udev/rules.d/50-rbd.rules` only if the distribution's Ceph
packages do not already provide an equivalent rule.

## Attachment and mapping lifecycle

The control plane owns the desired attachment ID, source, read-only setting,
and disk order. On a node, the storage provider owns mapping and cleanup
bookkeeping; Cloud Hypervisor only receives the resolved path. The lifecycle
is:

1. **Requested** — the manifest names a URI-backed attachment.
2. **Acquiring** — the storage provider records a VM lease before starting map
   or path-resolution work. A failed or cancelled create therefore cannot
   silently lose track of a partial mapping.
3. **Attached** — the resolved path is passed to Cloud Hypervisor. Multiple
   Odorobo VMs using the same canonical RBD resource share one provider
   acquisition and increment its reference count.
4. **Releasing** — after the VM process has exited, teardown releases that
   VM's lease. An Odorobo-owned mapping is unmapped only when its final active
   lease is released. A mapping that existed before Odorobo acquired it is
   borrowed and is never automatically unmapped by Odorobo.
5. **Released** — the lease is removed. Repeated cleanup of an already-released
   lease is a no-op.

A confirmed failure to map can be retried. If a command's result is ambiguous
(for example, the command timed out but the mapping may have been created),
Odorobo quarantines the resource rather than guessing who owns it: it blocks
reuse and automatic unmapping and reports that operator reconciliation is
required. Do not manually unmap a device while an affected VM may still be
running. The in-memory lease ledger is not persisted across agent restarts, so
inventory and reconcile outstanding RBD mappings before restarting an agent
that has active or uncertain storage operations.

### Operation deadlines

Storage work runs in background tasks; callers (VM create and delete) only
wait for it up to a deadline. The deadline is derived from the storage command
limits rather than picked by hand: each `rbd`/`iscsiadm` invocation gets 30
seconds plus a 2-second reap grace, and one disk resolution can run up to four
of them (RBD `device list`, `device map`, `device list`, then another
`device list` after up to 10 seconds of waiting for the udev path), plus up to
four 5-second iSCSI DNS lookups. That gives a per-operation budget of about
158 seconds. VM create applies it to each disk separately, and Cloud
Hypervisor's create timeout is 60 seconds on top of every disk's budget, so a
slow but working map isn't torn down as a failed create. The scheduler keeps a
pending placement for the VM's whole worst-case startup plus two minutes
before rescheduling it, since the create request itself times out after 120
seconds while the agent is still starting the VM. VM delete releases
disks one after another, so its deadline is one budget per lease (and two per
still-pending resolution, which must finish and may add a lease).

A "Storage operation deadline exceeded" error does not mean the operation
failed or was cancelled. The work is still running and the agent keeps the VM's
leases and any quarantine while it does. Retrying the delete (or the teardown
after a failed create) joins the in-flight cleanup instead of starting a second
one, and a new create for the same VM is refused until the earlier work
finishes. Do not reuse or manually unmap/log out the storage while it is still
in flight.

VM shutdown/deletion releases VM attachments according to the VM lifecycle,
but does not delete backing images. A VM's configured disk order is preserved;
the first manifest storage entry is the preferred boot disk.

## Mocking and tests

Storage backends implement a mockable provider interface for acquisition,
resolution, and release. Lifecycle tests use fake providers to cover shared
leases, borrowed mappings, partial failures, ambiguous command outcomes,
retries, cancellation, and idempotent cleanup; they do not require a live Ceph
cluster. The local Ceph development environment is documented in
[`.local/dev/README.md`](../.local/dev/README.md).
