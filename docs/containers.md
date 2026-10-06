# Container support in Odorobo

Odorobo runs OCI container workloads inside hardware-isolated microVMs: the image supplies the guest's root filesystem, while the host prepares and serves it over virtiofs. This document describes the container execution model and its storage behavior; it is not a general-purpose Docker-compatible runtime specification.

**Status:** Implemented for issues #112 and #113 on `caleb/containers`. Unit and privileged single-node integration tests cover layer materialization/composition, write modes, and the disposable RBD lifecycle. Cross-node fencing/drain still requires a multi-host test. This is the single source of truth for container support, image/layer caching, write modes, and persistent state. Future distributed-storage exploration is separate in [future-distributed-storage-design.md](future-distributed-storage-design.md).

## At a glance

The guest sees an ordinary virtiofs root and does not need composefs support.

- **Immutable base:** cache OCI layers locally by digest, compose each unique layer as a read-only, fs-verity-protected composefs image, and stack the layers with OverlayFS. Do not retain a separate flattened rootfs per image.
- **Write modes:** `read_only` allows only bounded VM-private scratch; `ephemeral` is a fully writable per-VM overlay removed at stop and is the default; `persistent` retains a per-VM writable upper across stop/restart.
- **Storage:** OCI/layer caches stay node-local. Persistent state supports a local backend for development and testing, and an RBD backend for cross-node cold restart. Live migration remains unsupported.
- **Reference aliases:** each normalized OCI source string gets a cached layout alias. Mutable tags are not refreshed automatically; persistent state separately pins the selected manifest digest.

## Goals and scope

- Boot an OCI image as a microVM root over virtiofs.
- Avoid storing a separate flattened rootfs for every image/VM; reuse identical OCI layers and cache immutable content on each node.
- Keep immutable root content read-only, digest-pinned, and shared; keep every VM's writes private.
- Provide read-only-with-scratch, writable ephemeral, and writable persistent modes.
- Support cross-node **cold restart** for persistent mode using Ceph RBD. Live migration remains unsupported.
- Retain a node-local backend for simple development, testing, and standalone deployments.
- Keep CephFS, Ceph Object Gateway, Podman, and a new distributed filesystem out of this implementation. Future distributed-storage options are documented separately.

## Architecture decision

Use **node-local OCI layer and composefs caches** plus **one of two per-VM persistence backends**:

1. `local`: node-local directories, suitable for all local tests and same-node restarts.
2. `rbd`: one RBD-backed filesystem per persistent VM, mounted by the host and used for that VM's writable upper. This is the backend for cross-node cold restart and maintenance drain.

The immutable base is not stored on Ceph for this implementation. Each node can warm/populate its local cache independently. Ceph RBD is used only for persistent mutable VM state. This avoids requiring CephFS fs-verity support or putting immutable image reads on a network filesystem.

## OCI pull, cache, and layer composition

1. Resolve the requested OCI reference to a concrete OCI manifest digest and the platform selected for the guest kernel. A mutable tag is an input alias, not the immutable cache identity. Persistent VM state records the resolved base digest.
2. Verify descriptor digests for the index/manifest/config and every layer blob. Cache compressed OCI blobs by SHA-256 digest in the **node-local** image cache. Refs that use the same layer blob should reuse it.
3. For each unique layer digest, securely unpack the layer into a temporary layer tree. Preserve OCI layer semantics: translate `.wh.<name>` to OverlayFS xattr whiteouts and `.wh..wh..opq` to opaque-directory metadata; apply whiteouts after extracting the same layer so same-layer files take precedence. Reject path traversal and symlink-based escapes. Enforce the compressed blob cap while skopeo writes its temporary OCI layout, plus per-layer/aggregate uncompressed-byte and tar-entry limits before publishing layer content.
4. Build a read-only composefs image for that layer, with a layer-specific content store, then delete the temporary unpacked tree. Cache the result by OCI layer digest. Enable and verify fs-verity for the composefs metadata image and required backing objects; fail closed if the configured cache filesystem cannot provide the required integrity guarantees.
5. Mount cached composefs layers read-only and stack them in OCI order (top layer first in the lowerdir list). Writable modes use OverlayFS with a per-VM upper/work. Read-only mode uses a lower-only OverlayFS for multiple layers; when there is exactly one layer, it uses a private read-only bind mount because Linux rejects a one-lower, lower-only OverlayFS mount. The resulting immutable base is exported over virtiofs.
6. Each node has a local fs-verity-capable cache filesystem (ext4 or btrfs by default). Cache and layer mounts are shared by VMs on that node. The implementation hard-links each verified compressed blob into `/var/lib/odorobo/oci-blobs/sha256` from the per-reference OCI layout, so identical descriptors share an inode. Each alias is kept under a sanitized, hashed ref key; tags are not refreshed automatically. Clear the per-ref alias to pull a mutable tag again. Persistent VMs reject a different selected manifest digest.

### Deduplication boundary

The first implementation guarantees whole-layer reuse: identical OCI layer digests share a cached composefs layer and its object store. It does **not** add a global composefs object store shared across different layer digests. That would deduplicate identical file payloads found in non-identical layers, but adds shared-object publication and reference-aware garbage collection. Defer it until measurements with representative images show enough extra savings to justify that complexity.

The temporary extraction tree is deleted after each layer is built and is not retained as a flattened image copy. The raw compressed-blob CAS and per-layer composefs cache are separate representations. Docker tags are inspected first and the copy is pinned to the selected manifest digest; the descriptor sizes are checked before the blob copy. Skopeo copies layers serially under a per-file limit while a live aggregate-byte monitor enforces the image cap. Rootfs startup logs the referenced unique compressed layer bytes, composefs metadata-image bytes, and composefs object-store payload bytes separately; these are per-image figures, not node-wide physical-usage totals. The compressed-byte cap also covers manifest/config blobs in the temporary OCI layout and removes the temporary layout if exceeded. There is currently no automatic cache eviction or garbage collector: this avoids deleting active or persistent-pinned content but means node cache usage grows until an operator performs safe maintenance cleanup. Do not remove mounted layers or content needed by persistent VM base digests.

## Rootfs write modes

The manifest uses `desired.rootfs.mode`; omission defaults to `ephemeral`. For compatibility only, legacy `read_only: true/false` is accepted when `mode` is absent and maps to `read_only`/`ephemeral`; specifying both (including `null`) is rejected. Serialization always emits canonical `mode`.

| Mode | Base and writes | Lifecycle |
|---|---|---|
| `read_only` | Immutable merged OCI lower with no writable root upper. VM-private tmpfs scratch is mounted at `/tmp`, `/run`, and `/var/tmp`; other root writes fail. | Scratch is volatile and disappears at stop. |
| `ephemeral` | Immutable lower plus a unique per-VM OverlayFS upper/work. This is the default when mode is omitted. | Upper/work are removed at VM teardown. |
| `persistent` | Immutable lower plus a unique persistent upper/work for this VM. | Upper survives stop/restart and is removed only by explicit VM deletion. |

Scratch defaults are bounded (`/tmp` 64 MiB, `/run` 32 MiB, `/var/tmp` 32 MiB), configurable by node policy through `ODOROBO_ROOTFS_TMP_SIZE`, `ODOROBO_ROOTFS_RUN_SIZE`, and `ODOROBO_ROOTFS_VARTMP_SIZE` (`M`/`G` values, 1 MiB–2 GiB each), and volatile. Startup validates all three paths before mounting any tmpfs, and rejects an absent, non-directory, or symlink mount point; this avoids partial mount leaks, mutating the shared image, or traversing an image-controlled path.

Read-only scratch mounts use the VM's private root mount view; mount propagation is made recursively private before the nested tmpfs mounts are installed. The actor mounts them before virtiofsd serves the root, then unmounts scratch paths before the root. Missing or invalid mount paths are all detected before mounting the first scratch filesystem, and partial mount failures unwind in reverse order.

For compatibility, accept the current legacy `read_only` boolean only when `mode` is absent: `true` maps to `read_only`, `false` maps to `ephemeral`. Reject manifests that specify both. Serialize the canonical `mode` form and deprecate the boolean for removal in a later manifest version.

## Persistent-state backends

Backend selection is node/operator configuration, not part of an application image reference:

- `local`: store persistent upper state under `/var/lib/odorobo/persistent-rootfs/<vmid>`, outside the per-boot VM runtime directory. A node-local `flock` prevents concurrent writers on this node. This backend enables development and lifecycle tests, but is node-affine and does **not** satisfy cross-node maintenance drain.
- `rbd`: provision/map one RBD image per persistent VM, require Ceph's `exclusive-lock` feature, format ext4, run `e2fsck -p` on reattach, and mount it on the node. The persistent OverlayFS upper lives on that filesystem; recreate a clean work directory before each mount after confirming no stale mount remains. RBD locking plus stop/unmount/unmap fencing prevents ordinary concurrent writers; real cross-host fencing must be validated in a multi-node deployment. Size is bounded by `ODOROBO_ROOTFS_RBD_SIZE` (default `1G`, configurable `64M`–`64G`). `ODOROBO_ROOTFS_BACKEND=local|rbd` selects the persistent backend (default `local`); `ODOROBO_ROOTFS_RBD_POOL` chooses the pool (falls back to `CEPH_POOL`, then `odorobo-blockpool`). `CEPH_CONFIG`, `CEPH_ID`, `CEPH_KEYFILE`, and `CEPH_CLUSTER` configure the Ceph client.

Before starting a persistent VM, write/validate state metadata including the state format version, VM identity, selected OCI manifest digest, backend, and OverlayFS compatibility setting. Refuse to mount a persisted upper over a different base digest unless an explicit reset/migration operation is implemented. The current API fails closed; reset/migration tooling is not yet provided. A failed DeleteVM cleanup is returned as an error; the AgentActor keeps inventory and the VM actor remains retryable until cleanup succeeds.

### RBD cold-restart and drain lifecycle

1. On the source, stop the guest cleanly and quiesce virtiofsd.
2. Unmount the per-VM merged root and layer mounts; flush/unmount the persistent filesystem; unmap the RBD device.
3. Confirm the source no longer owns a writable mount before allowing the volume to be attached elsewhere. Never rely on ext4 being safe for concurrent read-write mounts on multiple nodes; use explicit ownership/fencing in addition to Ceph's RBD locking mechanisms.
4. On the destination, map and mount the same RBD image, validate its state metadata and base OCI digest, rebuild/warm the local immutable layer cache as needed, then start a new VM actor with that upper.
5. On explicit DeleteVM, first complete unmount/unmap cleanup, then delete the RBD image. Ordinary stop must retain it. The request waits for guest quiescence and rootfs cleanup; shutdown, unmount, or volume-deletion failures are returned to the caller and leave the actor/state reachable for retry.

Live migration remains rejected for all OCI-rootfs VMs. The RBD path provides persistence across a cold stop/detach/reattach/restart, not device-state migration.

## Integrity, isolation, and process model

- OCI index/manifest/config/layer descriptor SHA-256 checks and config DiffID checks protect pull/materialization input. Defaults bound downloaded OCI blobs to 16 GiB per image, uncompressed layers to 8 GiB each, aggregate uncompressed image content to 32 GiB, and tar entries to 1,000,000; node policy may tune these using `ODOROBO_ROOTFS_MAX_COMPRESSED_BYTES`, `ODOROBO_ROOTFS_MAX_LAYER_BYTES`, `ODOROBO_ROOTFS_MAX_IMAGE_BYTES` (`M`/`G`/`T`) and `ODOROBO_ROOTFS_MAX_ENTRIES` (1–10,000,000). `mkcomposefs` stores filesystem metadata in EROFS and non-empty file payloads in the layer digest store; file metadata/xattrs (including OverlayFS markers) are part of the composefs image. The measured composefs image digest pins metadata. fs-verity on the image and backing objects protects metadata and payload bytes; mounts require both the digest pin and `verity` checks, and fail closed if unsupported. Device nodes/FIFOs under `/dev` are omitted because guest devtmpfs supplies `/dev`; other unsupported tar entry types reject the layer rather than silently changing its contents.
- All immutable layer mounts and merged lower mounts are read-only. A VM can only write to its own upper or explicitly allowed scratch tmpfs paths.
- Each VM's writable upper/work is distinct, including VMs with the same OCI manifest digest.
- The VM actor supervises virtiofsd with `--sandbox=chroot`, starts it before Cloud Hypervisor connects, stops it before unmount, and cleans up partial startup failures.
- Rootfs VMs require shared guest memory for the vhost-user virtiofs device. The direct-boot kernel/cmdline and `ttyS0` UART console remain as documented for issue #112. The guest does not need composefs support.
- Rootfs VMs reject live migration. Planned host drain uses clean cold restart with the same persistent upper and pinned OCI base digest.

## Storage choices for this implementation

- Persistent rootfs state supports both `local` and `rbd` backends. Use `local` for development and tests; use RBD when the persistent upper must follow a VM across nodes.
- OCI blobs, per-layer composefs images, and their content stores remain node-local for both backends.
- CephFS, Ceph Object Gateway, Podman, and a custom distributed storage system are not dependencies of this work. Consider them only through the separate [future storage design](future-distributed-storage-design.md).
- A global composefs CAS shared across non-identical layer digests is deferred; add only if measurements justify its garbage-collection and concurrency complexity.

## Verification results

Validated on the single-node privileged `.local/dev` Ceph environment (RBD image size set to a disposable `128M`):

- `cargo test --workspace`: passed; five privileged/KVM tests are ignored by default.
- `cargo clippy --workspace --all-targets --all-features`: passes with the workspace's current pedantic warnings.
- The ignored actor KVM test was run and passed: direct custom-kernel boot reached virtiofs through actor-supervised chroot virtiofsd, and explicit `DeleteVM` cleaned the mount/socket. The test uses a static fixture init binary built with Zig and needs nested KVM plus the host tools.
- Privileged OCI end-to-end test passed all three modes: per-VM bounded scratch and read-only write rejection; ephemeral isolation/removal; local persistent stop/restart, base-digest mismatch rejection, and explicit deletion.
- Privileged layer mount and OverlayFS integration tests passed: composefs/fs-verity image and payload verification, top-layer order, replacement, whiteout, and opaque-directory behavior.
- Disposable per-VM RBD helper test passed formatting, filesystem check, exclusive-lock feature verification, unmount/unmap/remap, persisted-upper reattach, and explicit volume deletion. A separate manual disposable RBD OverlayFS test also passed.
- Identical blob layouts share a hardlink inode in the raw OCI CAS; repeated identical layer digests resolve to the same composefs cache path. Per-image referenced compressed blob, composefs metadata, and object-store byte counts are logged.

The Ceph environment has one node only: this verifies cold detach/reattach on one host but does not prove cross-node lock fencing or failover. The node-local backend remains node-affine. There is no automatic node-cache eviction/GC; do not remove content used by active mounts or persistent bases. The microVM kernel script resolves the latest kernel.org longterm release dynamically; the dev host already had the resulting `vmlinux` built. A global composefs object store across distinct OCI layer digests remains deferred.
