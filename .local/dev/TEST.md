# End-to-end test: firmware boot from a Ceph RBD disk

`test.sh` boots a real OS image — Fedora 44 Cloud Base — through
[rust-hypervisor-firmware](https://github.com/cloud-hypervisor/rust-hypervisor-firmware)
(UEFI/PVH, no direct kernel boot) whose root disk is an `rbd://` image in the
in-container Ceph cluster. It proves the `rbd://` storage path end to end: the
agent maps the RBD to a kernel block device, resolves the URI, and hands the
disk to Cloud Hypervisor; the firmware then finds the GPT/ESP on that disk,
loads GRUB, and boots the kernel.

The whole test is one script, run from the host:

```sh
sudo bash .local/dev/init.sh     # one-time: build + start the stack
sudo bash .local/dev/test.sh     # the e2e test
```

## What the script does

1. **Preflight** — checks root, the container engine (podman, falling back to
   docker), that `odorobo-ceph` is running+healthy and `odorobo` is running,
   that the agent answers `/health`, and that the `rbd`/`loop` kernel modules
   are loaded.
2. **Assets** — downloads the firmware (`hypervisor-fw` ELF, 0.5.0 release) and
   the Fedora image into `test-assets/`, verifying the image sha256 (pinned)
   and the firmware size/ELF magic. Both are cached, so re-runs are fast.
3. **RBD write** — resizes `odorobo-blockpool/dev-disk` if it is smaller than
   the 5120 MiB image (the uncompressed size is read from the xz index, no
   decompression needed), maps it, and writes the image with a plain `dd`
   **from the host** into `/dev/rbd/odorobo-blockpool/dev-disk`. The mapping
   is a kernel object and `/dev` is shared with the host, so a host-side write
   lands in the pool. The script then unmaps, so the agent performs the mapping
   itself when the VM is created — the application path under test.
4. **VM create** — posts a manifest with `boot.firmware` set (no kernel, no
   cmdline) to the agent; the firmware path points at the repo-mounted copy
   (`/workspace/.local/dev/test-assets/hypervisor-fw`). A fresh ULID is
   generated per run.
5. **Boot watch** — polls the console history and reports each stage as it
   appears: firmware running → EFI partition/bootloader found on the RBD disk →
   kernel booting → login prompt.
6. **Interactive console** — attaches `socat` to
   `/run/odorobo/vms/<ulid>/console.sock` (the host's bind mount of the agent
   runtime dir). CH's serial socket serves one client at a time, so connecting
   takes the console over from the agent's spool. Log in with
   `fedora` / `fedora` (the documented base-image default; if a build locks
   password login, the login prompt itself still proves the OS booted),
   verify things by hand, detach with Ctrl-C.
7. **Cleanup** — deletes the VM through the agent and unmaps the RBD. The RBD
   image keeps the written Fedora image, so a follow-up run can skip the
   write: `SKIP_IMAGE_WRITE=1 sudo bash .local/dev/test.sh`.

## Overrides

| Variable | Default | Meaning |
| --- | --- | --- |
| `VM_VCPUS` | `2` | vCPU count |
| `VM_MEMORY_GB` | `2` | memory in GiB |
| `BOOT_TIMEOUT` | `240` | seconds to wait for the login prompt before attaching anyway |
| `KEEP_VM` | `0` | `1` leaves the VM running after detach (re-attach command is printed) |
| `SKIP_IMAGE_WRITE` | `0` | `1` skips the RBD write and reuses the previous image |
| `CEPH_POOL` / `CEPH_IMAGE_NAME` | `odorobo-blockpool` / `dev-disk` | match the compose defaults |

## Can the image be inserted into Ceph from the host?

Yes — that is what the script does. The `rbd` control commands (map/unmap/
info/resize) run in the container because the generated `ceph.conf` targets the
container's loopback (`127.0.0.1`). From the host you can run them directly
too, overriding the monitor address with the container's IP:

```sh
rbd --conf .local/dev/ceph/generated/ceph.conf --id=odorobo \
    --keyfile .local/dev/ceph/generated/client.odorobo.key \
    --mon-host "$(podman inspect --format '{{range $k, $v := .Network.Networks}}{{$v.IPAddress}}{{end}}' odorobo-ceph)" \
    device map odorobo-blockpool/dev-disk --options noudev
xz -d Fedora-....raw.xz | dd of=/dev/rbd/odorobo-blockpool/dev-disk bs=1M
```

The mapping is a kernel object, so it is visible from the host and the
container alike. For the test itself the script unmaps before creating the VM,
so the agent's own `rbd device map` is what runs (see README.md: "Do not map
the image from the host" refers to this application path).

## Expected console output

The firmware (which prints to the serial console) shows roughly:

```
[INFO] Booting with ...
[INFO] Virtio block device configured. Capacity: N sectors
[INFO] Found EFI partition
[INFO] Filesystem ready
[INFO] Using EFI boot.
[INFO] Found bootloader: \EFI\BOOT\BOOTX64.EFI
[INFO] Executable loaded
```

then GRUB, the kernel banner (`Linux version ...`), systemd, and finally the
login prompt. The `AmazonEC2` variant's ESP ships GRUB at the standard
`/EFI/BOOT/BOOTX64.EFI` fallback path, which is what the firmware loads.

## Troubleshooting

- **Agent rejects the manifest** — the response is printed; the usual causes are
  a missing firmware file (check `test-assets/hypervisor-fw` exists and the
  repo is mounted at `/workspace`) or a stale RBD mapping.
- **No boot output at all** — check the agent logs:
  `podman compose -f .local/dev/compose.yml logs --tail=200 odorobo`.
- **Firmware finds the disk but GRUB hangs** — the firmware's EFI environment
  is minimal (developed against Ubuntu's shim+GRUB2); if a different guest
  image misbehaves, compare against the direct-kernel-boot path or use an
  edk2/CLOUDHV firmware instead.
- **Stale mappings after a reset** — the script unmaps before writing; if you
  mapped manually, unmap first or the agent will reuse the broken device.
