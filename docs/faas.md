# FaaS microVMs: OCI rootfs over virtiofs (issue #112)

FaaS functions boot OCI container images directly as the guest root
filesystem — no disk image, no bootloader. The node materializes the image
as a **digest-pinned composefs mount** and serves it to the guest over
virtiofs; the guest boots a tiny custom kernel straight into a
`root=rootfs rootfstype=virtiofs` mount. VMs of the same image share one
content-addressed store on the node, so N functions of the same image cost
one unpack.

## Architecture

```
manifest.desired.rootfs ──► agent (VM actor)
                              │
                              ▼
                 faas::mount_rootfs()            /var/lib/odorobo/
                   1. skopeo copy ──────────►  oci-cache/<ref>/        (OCI layout, per ref)
                   2. unpack layers ────────►  scratch tree           (tar, gzip/zstd, whiteouts,
                   3. mkcomposefs ──────────►  roots/<digest>/          sha256-verified layers)
                      │                        ├─ root.cfs            (composefs image, fs-verity)
                      └── fsverity enable ──►  └─ store/             (content-addressed objects)
                              │
                              ▼
                 mount.composefs -o basedir=…,digest=…  (per-VM mount, verified by digest)
                              │
                              ▼
                 VMActor spawns + supervises virtiofsd ──── rootfs.sock (vhost-user)
                              │
                              ▼
                 Cloud Hypervisor ──fs tag=rootfs──► guest kernel (direct boot)
                            kernel cmdline: console=ttyS0 root=rootfs rootfstype=virtiofs rw
```

The guest never sees composefs. It mounts a plain virtiofs filesystem
tagged `rootfs`, so no initramfs and no composefs/erofs support is needed
in the guest kernel.

## Components

- **Manifest** (`odorobo/src/manifest.rs`): `desired.rootfs` is
  `{ oci: <image ref>, read_only: bool }`. The OCI reference is anything
  `skopeo copy` understands (a bare `registry/repo:tag` is assumed to be a
  docker reference). `read_only: true` (typical for FaaS) shares one
  read-only mount of the image across all VMs; `read_only: false` adds a
  per-VM overlayfs upper directory on the node (temporary — it dies with
  the VM).
- **Rootfs provisioning** (`odorobo/src/ch_driver/faas.rs`): pull, unpack,
  build, mount, unmount, plus the `VirtioFsSupervisor`.
- **Conversion** (`odorobo/src/ch_driver/manifest.rs`): a manifest with a
  `rootfs` converts to a CH config with the `fs` device (tag `rootfs`,
  socket in the VM runtime dir), `memory shared=on` (required by
  vhost-user), and — unless the manifest overrides boot intent — a direct
  kernel boot of `/var/lib/odorobo/microvm/vmlinux` with
  `console=ttyS0 root=rootfs rootfstype=virtiofs rw`.
- **Kernel** (`scripts/build-microvm-kernel.sh`): builds the tiny custom
  kernel (LTS, currently 6.18.x): x86_64_defconfig + PVH + virtio
  (pci/net/blk/console/balloon) + overlayfs + fuse/virtiofs + vsock, all
  built-in, with modules/debug/large-subsystem trims for fast boot.
  Installs to `/var/lib/odorobo/microvm/vmlinux`.

## Process model (pi-vm style)

The **VM actor supervises virtiofsd as an extra child process** for the
VM's lifetime: unexpected virtiofsd exits are restarted with capped
backoff; stopping the actor stops virtiofsd and unmounts the composefs
mount. Children are spawned `kill_on_drop`, so even an abrupt actor kill
does not leak processes. CH's child watcher (existing) stops the actor
when CH dies, which cascades the teardown.

## Guest requirements

Because the root is the OCI image itself, the guest init comes from the
image. Images that expect a normal userspace boot need `/sbin/init` (or
`/bin/sh`); override `boot.cmdline` in the manifest to steer it, e.g.
`root=rootfs rootfstype=virtiofs rw init=/bin/sh`. The microvm kernel
enables virtio, virtiofs, overlayfs, vsock and the 8250 UART console.

## Constraints

- **Live migration is not supported for rootfs VMs.** The virtiofs device
  is vhost-user, and vhost-user device state cannot be serialized into
  CH's migration stream. The driver rejects `MigrateVMReceive`/
  `PrepMigration` for rootfs manifests with an explicit error — reschedule
  cold instead: the destination node can mount the identical digest-pinned
  root.
- Pinned mounts require the store on an **fs-verity-capable filesystem**
  (ext4, btrfs).
- Tag drift: the per-ref pull cache (`oci-cache/`) and the per-digest root
  (`roots/<digest>/`) are reused until an operator clears them. Re-pulling
  a moving tag (e.g. `:latest`) requires deleting the cache entry.

## Node dependencies

```
dnf install skopeo composefs fsverity-utils virtiofsd
# + the cloud-hypervisor binary (not packaged in Fedora 44) and
# scripts/build-microvm-kernel.sh for /var/lib/odorobo/microvm/vmlinux
```

## Testing

- Unit: `cargo test -p odorobo --bin odorobo` (manifest conversion, layer
  unpacking/whiteouts, digest verification, supervisor restart/stop).
- Full boot through the actor (boots a real busybox-root VM under KVM):

```
cargo test -p odorobo --bin odorobo faas_vm_boots -- --ignored --nocapture
```
