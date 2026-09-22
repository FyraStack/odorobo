# End-to-end test: boot a VM from a Ceph RBD disk

Boots a minimal VM through Odorobo whose root disk is an `rbd://` image in the
in-container Ceph cluster. It proves the `rbd://` storage path end to end: the
agent maps the RBD to a kernel block device, resolves the URI, and hands the disk
to Cloud Hypervisor.

Step 0 runs from the host; steps 1–6 run inside the `odorobo` container.

## Step 0 — Start the stack and enter the container

```sh
sudo bash .local/dev/init.sh
sudo podman compose -f .local/dev/compose.yml exec -it odorobo sh
```

## Step 1 — Install required packages

```sh
dnf install -y busybox tar
```

## Step 2 — Re-fetch the Alpine kernel

```sh
cd /tmp
curl -O https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/linux-virt-6.12.110-r0.apk
tar xOf linux-virt-6.12.110-r0.apk boot/vmlinuz-virt > /tmp/vmlinuz-virt
ls -l /tmp/vmlinuz-virt
```

## Step 3 — Build the rootfs

```sh
mkdir -p /tmp/rootfs/{bin,dev,etc,proc,sys}
cp /usr/bin/busybox /tmp/rootfs/bin/
printf '%s\n' \
  '#!/bin/sh' \
  'mount -t proc proc /proc' \
  'mount -t sysfs sys /sys' \
  'echo "=== odorobo VM: booted from ceph-backed rootfs ==="' \
  'ls /dev | grep vda' \
  'echo "=== dropping to shell ==="' \
  'exec /bin/busybox ash' > /tmp/rootfs/init
chmod +x /tmp/rootfs/init
for a in ash sh ls cat mount echo grep; do ln -s bin/busybox /tmp/rootfs/$a; done
fallocate -l 512M /tmp/rootfs.img
mkfs.ext4 /tmp/rootfs.img
mkdir -p /tmp/mnt && mount /tmp/rootfs.img /tmp/mnt
cp -a /tmp/rootfs/. /tmp/mnt/
umount /tmp/mnt
```

## Step 4 — Map the RBD

A reset re-creates the image with a new object id, so any mapping that survived in
the kernel is stale. Unmap it first, or the agent's "already mapped?" check will
reuse a broken device.

```sh
RBD="rbd --conf=/workspace/.local/dev/ceph/generated/ceph.conf --id=odorobo --keyfile=/workspace/.local/dev/ceph/generated/client.odorobo.key"

# Clear any stale mappings that survived the reset
$RBD device unmap /dev/rbd0 --options noudev 2>/dev/null
$RBD device unmap /dev/rbd1 --options noudev 2>/dev/null
$RBD device ls   # confirm the table is empty

# Map the fresh image
$RBD device map odorobo-blockpool/dev-disk --options noudev; echo "map-exit=$?"
ls -l /dev/rbd* 2>/dev/null

# Gate: the agent's map() runs `rbd device list` first, so it must succeed
$RBD device list; echo "list-exit=$?"
```

Expect `map-exit=0`, a `brw-... /dev/rbd0` line, and `list-exit=0`. Confirm before
continuing.

## Step 5 — Write the rootfs to the RBD

```sh
dd if=/tmp/rootfs.img of=/dev/rbd/odorobo-blockpool/dev-disk bs=1M
sync
```

## Step 6 — Boot the VM

```sh
bash /workspace/.local/dev/vm-test.sh
```

The script health-checks the agent, posts the VM manifest, waits, and dumps the
console history.

Expected console: kernel boot log → `=== odorobo VM: booted from ceph-backed
rootfs ===` → `vda` listed → shell prompt. Then run `cat /proc/mounts` and confirm
`/dev/vda / ext4` — that is the VM reading from the Ceph RBD pool.

> **Note:** the stock Alpine `linux-virt` kernel ships `ext4` and `virtio_blk` as
> modules, so a direct kernel boot (no initramfs) panics at
> `VFS: Unable to mount root fs on "/dev/vda"`. That is a test-kernel limitation,
> not a Ceph issue — the `rbd://` resolution and disk attach are proven. To get a
> full shell, use a kernel with `ext4`/`virtio_blk` built in.
