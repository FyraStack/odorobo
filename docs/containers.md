# Container support in Odorobo

Odorobo runs OCI container workloads inside hardware-isolated microVMs: the image supplies the guest's root filesystem, while the host prepares and serves it over virtiofs. The target use case is PaaS/FaaS-style hosting: OCI-compatible workloads that need a hardware virtualization boundary instead of a shared kernel, using the images as they are. This document describes the container execution model and its storage behavior; it is not a general-purpose Docker-compatible runtime specification.

**Status:** Implemented for issues #112 and #113 on `caleb/containers`. Unit and privileged single-node integration tests cover image import and composition, write modes, and the disposable RBD lifecycle. Cross-node fencing/drain still requires a multi-host test. This is the single source of truth for container support, the composefs repository, write modes, and persistent state. Future distributed-storage exploration is separate in [future-distributed-storage-design.md](future-distributed-storage-design.md).

## At a glance

The guest sees an ordinary virtiofs root and does not need composefs support.

- **Immutable base:** import the image into one node-local composefs repository, which stores file payloads once per digest and gives each image a single fs-verity-protected EROFS metadata object. Mount that object with OverlayFS; do not retain a separate flattened rootfs per image.
- **Write modes:** `read_only` allows only bounded VM-private scratch; `ephemeral` is a fully writable per-VM overlay removed at stop and is the default; `persistent` retains a per-VM writable upper across stop/restart.
- **Storage:** the composefs repository stays node-local. Persistent state supports a local backend for development and testing, and an RBD backend for cross-node cold restart. Live migration remains unsupported.
- **Digest pinning:** an import resolves the source reference to a manifest digest first and pulls that digest, so a mutable tag changes content only on a later import. Persistent state separately pins the selected manifest digest.
- **No cleanup:** nothing evicts repository objects. Disk use only grows until an operator removes content that no mounted or persistent VM needs; there is no reference-aware garbage collector yet.
- **Init:** the guest boots a small initramfs whose PID 1 mounts the image's virtiofs root and runs the image's `ENTRYPOINT`/`CMD` with its `Env`, `WorkingDir` and `User`. `boot.cmdline` can still set `init=`. See [Guest boot and init](#guest-boot-and-init).
- **Prerequisites:** the node needs the microVM kernel and initramfs built by `scripts/build-microvm-kernel.sh` and `just -f odorobo-init/justfile initramfs`, plus `skopeo` and `virtiofsd`. `/var/lib/odorobo` should be on an fs-verity filesystem such as ext4 or btrfs so mounts verify content; otherwise imports mount unverified.

## Goals and scope

- Boot an OCI image as a microVM root over virtiofs.
- Avoid storing a separate flattened rootfs for every image/VM; deduplicate identical image content in one repository per node.
- Keep immutable root content read-only, digest-pinned, and shared; keep every VM's writes private.
- Provide read-only-with-scratch, writable ephemeral, and writable persistent modes.
- Support cross-node **cold restart** for persistent mode using Ceph RBD. Live migration remains unsupported.
- Retain a node-local backend for simple development, testing, and standalone deployments.
- Keep CephFS, Ceph Object Gateway, Podman, and a new distributed filesystem out of this implementation. Future distributed-storage options are documented separately.

## Architecture decision

Use a **node-local composefs repository** plus **one of two per-VM persistence backends**:

1. `local`: node-local directories, suitable for all local tests and same-node restarts.
2. `rbd`: one RBD-backed filesystem per persistent VM, mounted by the host and used for that VM's writable upper. This is the backend for cross-node cold restart and maintenance drain.

The immutable base is not stored on Ceph for this implementation. Each node imports its own repository independently. Ceph RBD is used only for persistent mutable VM state. This avoids requiring CephFS fs-verity support or putting immutable image reads on a network filesystem.

## OCI import and the composefs repository

Every image is imported into one node-local composefs repository at `/var/lib/odorobo/composefs`. The importer unpacks layer content into content-addressed, fs-verity-protected objects and gives each image a single small EROFS metadata object linked to its OCI config. This is the only immutable root store: there is no per-image flattened rootfs, no per-layer composefs cache, and no per-reference OCI layout.

1. **Preflight:** resolve the reference with `skopeo inspect --raw` to a concrete manifest digest and the platform selected for the guest kernel, and check its compressed size against the cap. A mutable tag is an input alias, not an identity: the import pulls the exact digest that was just inspected and verified.
2. **Import:** pull that digest into the repository through `containers-image-proxy`. The importer applies OCI changeset semantics itself and merges the image's layers in order into one EROFS metadata object; file payloads become repository objects, so identical content anywhere in the repository is stored once. Re-importing an image whose objects are already present adds nothing. The importer copies the root directory metadata from `/usr` for deterministic digests, so an image without a `/usr` directory is rejected.
3. **Mount:** every VM gets one OverlayFS mount whose lowers are that EROFS metadata object plus a per-VM directory holding the image's OCI config document. `read_only` mounts those two lowers with no writable layer; `ephemeral` and `persistent` add the per-VM upper/work. Whiteouts and opaque directories are resolved inside the imported image, so nothing layer-shaped reaches the guest and no `lowerdir` list, empty-lower workaround, or layer-count cap is involved.
4. **Config:** the image's OCI config document is read back from the repository as a content stream, written into the per-VM config directory, and shows up in the guest as `/.odorobo-config.json`.

`ODOROBO_ROOTFS_ALLOW_LOCAL_TRANSPORTS=1` additionally allows local transports such as `oci:` and `docker-archive:` for development.

### Deduplication boundary

Deduplication is repository-wide: two images that share file payloads share those objects even when their layers are not byte-identical, and re-importing an image is idempotent. Each image still has its own EROFS metadata object, so a repository grows with the image count plus the union of distinct file payloads. The import logs the compressed bytes pulled and the objects/bytes actually added, so a warm re-import is visible as zero new objects. There is currently no reference-aware garbage collector: do not remove repository objects while any root is mounted or any persistent upper pins its base digest.

## Rootfs write modes

The manifest uses `desired.rootfs.mode`; omission defaults to `ephemeral`. For compatibility only, legacy `read_only: true/false` is accepted when `mode` is absent and maps to `read_only`/`ephemeral`; specifying both (including `null`) is rejected. Serialization always emits canonical `mode`.

| Mode | Base and writes | Lifecycle |
|---|---|---|
| `read_only` | Immutable composefs image with no writable root upper. VM-private tmpfs scratch is mounted at `/tmp`, `/run`, and `/var/tmp`; other root writes fail. | Scratch is volatile and disappears at stop. |
| `ephemeral` | Immutable image plus a unique per-VM OverlayFS upper/work. This is the default when mode is omitted. | Upper/work are removed at VM teardown. |
| `persistent` | Immutable image plus a unique persistent upper/work for this VM. | Upper survives stop/restart and is removed only by explicit VM deletion. |

Scratch mounts always use `nosuid,nodev`, and VM runtime trees are host-private (0700). Scratch defaults are bounded (`/tmp` 64 MiB, `/run` 32 MiB, `/var/tmp` 32 MiB), configurable by node policy through `ODOROBO_ROOTFS_TMP_SIZE`, `ODOROBO_ROOTFS_RUN_SIZE`, and `ODOROBO_ROOTFS_VARTMP_SIZE` (`M`/`G` values, 1 MiB–2 GiB each), and volatile. Startup validates all three paths before mounting any tmpfs, and rejects an absent, non-directory, or symlink mount point; this avoids partial mount leaks, mutating the shared image, or traversing an image-controlled path.

Read-only scratch mounts use the VM's private root mount view; mount propagation is made recursively private before the nested tmpfs mounts are installed. The actor mounts them before virtiofsd serves the root, then unmounts scratch paths before the root. Missing or invalid mount paths are all detected before mounting the first scratch filesystem, and partial mount failures unwind in reverse order.

For compatibility, accept the current legacy `read_only` boolean only when `mode` is absent: `true` maps to `read_only`, `false` maps to `ephemeral`. Reject manifests that specify both. Serialize the canonical `mode` form and deprecate the boolean for removal in a later manifest version.

## Persistent-state backends

Backend selection is node/operator configuration, not part of an application image reference:

- `local`: store persistent upper state under `/var/lib/odorobo/persistent-rootfs/<vmid>`, outside the per-boot VM runtime directory. A node-local `flock` prevents concurrent writers on this node. This backend enables development and lifecycle tests, but is node-affine and does **not** satisfy cross-node maintenance drain.
- `rbd`: provision/map one RBD image per persistent VM, require Ceph's `exclusive-lock` feature and map with `--exclusive` (`lock_timeout=30`), format ext4, run `e2fsck -p` on reattach, and mount it on the node. The persistent OverlayFS upper lives on that filesystem; recreate a clean work directory before each mount after confirming no stale mount remains. The feature alone permits cooperative lock transfers and is not a filesystem ownership lock; exclusive mapping disables those transfers. Existing node mappings are rejected rather than adopted. Newly created volumes are published under their final VM identity only after storing an `uninitialized` RBD metadata phase; successful formatting records `formatted` before mounting or exposing guest writes. Retries may format only explicitly uninitialized volumes; legacy volumes without a phase are checked, never reformatted. Stop/unmount/unmap releases ownership; real cross-host fencing must still be validated in a multi-node deployment. Size is bounded by `ODOROBO_ROOTFS_RBD_SIZE` (default `1G`, configurable `64M`–`64G`). `ODOROBO_ROOTFS_BACKEND=local|rbd` selects the persistent backend (default `local`); `ODOROBO_ROOTFS_RBD_POOL` chooses the pool (falls back to `CEPH_POOL`, then `odorobo-blockpool`). `CEPH_CONFIG`, `CEPH_ID`, `CEPH_KEYFILE`, and `CEPH_CLUSTER` configure the Ceph client.

Before starting a persistent VM, write/validate state metadata including the state format version, VM identity, selected OCI manifest digest, backend, and OverlayFS compatibility setting. Refuse to mount a persisted upper over a different base digest unless an explicit reset/migration operation is implemented. The current API fails closed; reset/migration tooling is not yet provided. A failed DeleteVM cleanup is returned as an error; scheduler intent and agent inventory are retained, and the VM actor remains retryable until cleanup succeeds. OCI VM actors are lifecycle owners: cold shutdown detaches storage but retains the actor identity for restart/delete, and failed startup retains an error plus acquired-resource inventory. Durable scheduler inventory and etcd recovery are separate work; this is process-local lifecycle ownership. Teardown is idempotent and stops at the first failed dependency. Runtime purging is separate from VMM shutdown and refuses to recurse while any runtime mount remains.

### RBD cold-restart and drain lifecycle

1. On the source, stop the guest cleanly and quiesce virtiofsd.
2. Unmount the per-VM root; flush/unmount the persistent filesystem; unmap the RBD device.
3. Confirm the source no longer owns a writable mount before allowing the volume to be attached elsewhere. Never rely on ext4 being safe for concurrent read-write mounts on multiple nodes; use explicit ownership/fencing in addition to Ceph's RBD locking mechanisms.
4. On the destination, map and mount the same RBD image, validate its state metadata and base OCI digest, import the pinned base image into the local repository as needed, then start a new VM actor with that upper.
5. On explicit DeleteVM, first complete unmount/unmap cleanup, then delete the RBD image. Ordinary stop must retain it. The request waits for guest quiescence and rootfs cleanup; shutdown, unmount, or volume-deletion failures are returned to the caller and leave the actor/state reachable for retry.

Live migration remains rejected for all OCI-rootfs VMs. The RBD path provides persistence across a cold stop/detach/reattach/restart, not device-state migration.

## Guest boot and init

Cloud Hypervisor direct-boots the node's microVM kernel (`/var/lib/odorobo/microvm/vmlinux`, built by `scripts/build-microvm-kernel.sh`) with a small gzipped initramfs (`/var/lib/odorobo/microvm/initramfs.cpio.gz`, built by `odorobo-init/justfile`) and no firmware. The default command line is `console=ttyS0`.

The initramfs contains a single static `odorobo-init` binary as `/init`. As PID 1 it:

1. mounts `devtmpfs`, `proc` and `sysfs`;
2. mounts the virtiofs share tagged `rootfs` at `/newroot` and reads the image's OCI config document from `/.odorobo-config.json` inside it;
3. creates the scratch mount points, moves the pseudo-filesystems into `/newroot`, and `chroot`s into it;
4. resolves the config's `User` against the image's `/etc/passwd` and `/etc/group`, applies `setgroups`/`setgid`/`setuid`, builds `Env` plus the entrypoint and command, and `execve`s it in the config's `WorkingDir`;
5. reaps zombies, forwards `SIGTERM`/`SIGINT`/`SIGQUIT`/`SIGHUP` to the workload's process group, and powers the VM off when the workload exits.

If the image has neither an entrypoint nor a command, the init falls back to `/sbin/init`, `/etc/init`, `/bin/init` and then `/bin/sh`. Unknown or missing config fields are ignored; an unreadable config is fatal. The agent writes `.odorobo-config.json` into `/run/odorobo/vms/<vmid>/config` and mounts that directory as a lower layer before start, so the guest never sees the network or the host filesystem.

The console is the 16550 serial port (`ttyS0`). The agent exposes it as a socket and keeps a history, read through `GET /vms/{vmid}/console/history`. The virtio console (`hvc0`) is off.

Rootfs VMs can publish guest ports on the host with `ports` on a manifest network. The host resolves the guest address from its DHCP lease and installs DNAT rules; see [networking.md](networking.md#publishing-container-ports).

**Overrides:** `boot.kernel` and `boot.initramfs` replace the defaults, and `boot.cmdline` replaces the whole command line. Overriding the initramfs means supplying another image that mounts the virtiofs root itself; overriding the kernel must keep built-in `virtiofs`, `overlayfs` and `devtmpfs`.

The ignored KVM test (`oci_rootfs_vm_boots_through_the_actor`) compiles a static C program with Zig, installs it as `/bin/sh` in a fixture image whose OCI config sets `Entrypoint: ["/bin/sh"]`, and boots it through the actor. The test passes when the console shows the program's message, which proves the config lower, the initramfs and the virtiofs root all work together.

## Integrity, isolation, and process model

- Imports are digest-pinned: `skopeo inspect --raw` resolves the platform manifest first and the pull copies exactly that digest, so a tag that moves mid-import cannot substitute content. Defaults bound downloaded compressed data to 16 GiB per image (`ODOROBO_ROOTFS_MAX_COMPRESSED_BYTES`, `M`/`G`/`T`). The older per-layer, aggregate-uncompressed and tar-entry limits were removed together with the host-side unpacking path, since nothing is unpacked into a host-visible tree anymore. `composefs-oci` materializes the merged image: file metadata, xattrs and directory structure live in the EROFS metadata object, and file payloads become repository objects. fs-verity covers metadata and payloads when the repository filesystem supports it, and mounts verify the image object digest; a repository on a filesystem without fs-verity mounts unverified.
- The composefs image mount and every exported root are read-only. A VM can only write to its own upper or explicitly allowed scratch tmpfs paths.
- Each VM's writable upper/work is distinct, including VMs with the same OCI manifest digest.
- The VM actor supervises one virtiofsd process with `--sandbox=chroot` over the mounted root, starts it before Cloud Hypervisor connects, stops it before unmounting the root and its lowers, and cleans up partial startup failures.
- Rootfs VMs require shared guest memory for the vhost-user virtiofs device. The direct-boot kernel/cmdline and `ttyS0` UART console remain as documented for issue #112. The guest does not need composefs support.
- Rootfs VMs reject live migration. Planned host drain uses clean cold restart with the same persistent upper and pinned OCI base digest.

## Storage choices for this implementation

- Persistent rootfs state supports both `local` and `rbd` backends. Use `local` for development and tests; use RBD when the persistent upper must follow a VM across nodes.
- The composefs repository remains node-local for both backends.
- CephFS, Ceph Object Gateway, Podman, and a custom distributed storage system are not dependencies of this work. Consider them only through the separate [future storage design](future-distributed-storage-design.md).
- Reference-aware garbage collection for the repository is deferred; add only if measurements justify its complexity. Whole-image imports are already idempotent and payloads are shared across images.

## Verification results

Recorded on the development host with a rootful debug agent, plus the ignored privileged tests run directly as root.

- `cargo test -p odorobo`: 95 passed, 5 ignored (privileged/KVM/RBD). `cargo clippy -p odorobo -p odorobo-init --all-targets` and `cargo fmt --all --check` are clean apart from the pre-existing macro-semicolon warnings.
- Boot, stop and delete end to end: an ephemeral `docker://traefik/whoami:latest` VM imported 347 objects / 12,890,895 bytes from 5,139,859 compressed bytes across 3 layers, created one EROFS image object, mounted it, and answered `whoami` over the guest network. Deleting both test VMs left no runtime directories, mounts, published-port rules or `virtiofsd` processes.
- Deduplication boundary: re-importing the same image logged `new_objects: 0, new_bytes: 0` while the repository stayed at 14 MB / 358 objects.
- The ignored KVM actor test (`oci_rootfs_vm_boots_through_the_actor`) passes: the guest runs `/init` from the initramfs, mounts the composefs image over virtiofs, reads the OCI config from `/.odorobo-config.json`, executes the fixture entrypoint, then exercises busy-root teardown retention across `ShutdownVM`, restart, a hypervisor crash and `DeleteVM`.
- Privileged mount tests pass: write-mode isolation and removal for `read_only`, `ephemeral` and `persistent`, failed-start inventory retention, and purge protection while the root mount is busy.
- The disposable per-VM RBD helper test (earlier run on the same host) passed formatting, filesystem check, exclusive-lock feature verification, unmount/unmap/remap, persisted-upper reattach, and explicit volume deletion.

The Ceph environment has one node only: this verifies cold detach/reattach on one host but does not prove cross-node lock fencing or failover. The node-local backend remains node-affine. There is no automatic repository garbage collection; do not remove objects used by active mounts or persistent bases. The microVM kernel script resolves the latest kernel.org longterm release dynamically; the dev host already had the resulting `vmlinux` built.
