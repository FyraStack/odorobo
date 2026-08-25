# Local Ceph with cephadm

These scripts let you run a single host ceph cluster via cephadm (uses podman and quay.io/ceph/ceph internally). 
It creates a 10gb sparse raw file that is then used as a `/dev/nbdN` block device on your host system. 
You can then run odorobo on your host against this ceph cluster.

## Prerequisites

On Fedora:

```bash
sudo dnf install -y cephadm ceph-common podman cloud-hypervisor qemu-img qemu-nbd kmod iproute socat
sudo modprobe rbd nbd
```
## Usage

There are 4 scripts for controlling the cluster.

- `init.sh` creates a new cluster if needed, provisions the virtual OSD, and creates the Odorobo pool/client/image.
- `start.sh` starts an existing stopped cluster without provisioning or deleting data.
- `stop.sh` stops an existing cluster while preserving its data.
- `reset.sh` destructively removes the cluster and local virtual-disk state.

Use `init.sh` once for a new checkout or after `reset.sh`.

Then export the following environment variables so odorobo can connect to it.

```bash
export CEPH_CONFIG="$PWD/.local/ceph/generated/ceph.conf"
export CEPH_ID=odorobo
export CEPH_KEYFILE="$PWD/.local/ceph/generated/client.odorobo.key"
export CEPH_CLUSTER=ceph
```






## Reference

### What `init.sh` does

`init.sh` is the provisioning command. On a new machine, it:

1. Bootstraps a single-host Ceph cluster with `cephadm` if `/etc/ceph/ceph.conf` does not exist.
2. Runs the Ceph MON, MGR, and OSD daemons in rootful Podman containers.
3. Creates or reattaches the sparse OSD image at `.local/ceph/state/osd.raw`.
4. Exposes that image to the host as a `/dev/nbdN` block device using `qemu-nbd`.
5. Creates an OSD on that virtual block device.
6. Creates the `odorobo-blockpool` RBD pool with single-node development settings.
7. Creates the restricted `client.odorobo` client.
8. Creates the `dev-disk` RBD image.
9. Writes the host-side Ceph configuration and client key under `.local/ceph/generated/`.

`init.sh` does not reuse or wipe existing virtual-disk state. If a previous attempt leaves `.local/ceph/state/osd.raw` or `.local/ceph/state/osd.device`, it stops and directs you to `reset.sh`. This keeps initialization deterministic and destructive cleanup in the reset lifecycle.

The OSD image is sparse, so its virtual size defaults to 10 GiB but it does not immediately consume 10 GiB of physical storage.

The virtual disk and cluster are intentionally suitable only for local development. This setup does not provide production-level redundancy, quorum, or failure recovery.

### Verify the cluster

After `init.sh` or `start.sh`, check that the Ceph containers and daemons are running:

```bash
sudo podman ps \
  --format 'table {{.Names}}\t{{.Image}}\t{{.Status}}'

sudo ceph -s
sudo ceph osd tree
```

A one-node development cluster may report `HEALTH_WARN`. Warnings about a single monitor, low monitor disk space, or reduced redundancy are expected. The OSD should be `up` and `in`.

Check the virtual OSD device:

```bash
cat .local/ceph/state/osd.device
lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS
```

The device recorded in `osd.device` should be the device containing the `ceph_bluestore` mapping.

### Verify the Odorobo client and RBD image

Export the generated credentials if you have not already done so:

```bash
export CEPH_CONFIG="$PWD/.local/ceph/generated/ceph.conf"
export CEPH_ID=odorobo
export CEPH_KEYFILE="$PWD/.local/ceph/generated/client.odorobo.key"
export CEPH_CLUSTER=ceph
```

List the pool contents:

```bash
sudo -E rbd \
  --conf="$CEPH_CONFIG" \
  --id="$CEPH_ID" \
  --keyfile="$CEPH_KEYFILE" \
  ls --pool odorobo-blockpool
```

Expected output:

```text
dev-disk
```

Inspect the client permissions:

```bash
sudo ceph auth get client.odorobo
```

The client should have read-only monitor access and read/write access limited to `odorobo-blockpool`.

### Test host-side RBD mapping

Odorobo currently invokes the host's `rbd` command and expects the Linux kernel RBD module to create a local block device. The host therefore needs `ceph-common`, the `rbd` kernel module, and the udev rule described in `docs/storage.md`.

If the udev rule is not already installed by the distribution's Ceph packages:

```bash
sudo tee /etc/udev/rules.d/50-rbd.rules >/dev/null <<'EOF'
KERNEL=="rbd[0-9]*", ENV{DEVTYPE}=="disk", PROGRAM=="/usr/bin/ceph-rbdnamer %k", SYMLINK+="rbd/%c"
KERNEL=="rbd[0-9]*", ENV{DEVTYPE}=="partition", PROGRAM=="/usr/bin/ceph-rbdnamer %k", SYMLINK+="rbd/%c-part%n"
EOF

sudo udevadm control --reload-rules
sudo udevadm trigger
```

Map the test image:

```bash
sudo -E rbd device map odorobo-blockpool/dev-disk \
  --conf="$CEPH_CONFIG" \
  --id="$CEPH_ID" \
  --keyfile="$CEPH_KEYFILE"
```

Verify the stable device path:

```bash
ls -l /dev/rbd/odorobo-blockpool/dev-disk
sudo rbd device list
```

When finished, unmap the image before stopping or resetting Ceph:

```bash
sudo -E rbd device unmap odorobo-blockpool/dev-disk \
  --conf="$CEPH_CONFIG" \
  --id="$CEPH_ID" \
  --keyfile="$CEPH_KEYFILE"
```

The helper scripts also unmap RBD devices automatically:
Stop the cluster while preserving its data:

```bash
bash .local/ceph/stop.sh
```

This unmaps RBD devices and stops the cephadm systemd target for the whole cluster without deleting its data.

### Recover from an interrupted initialization

The initializer intentionally does not recover partial virtual-OSD state. If it fails or is interrupted, run the destructive reset before retrying:

```bash
bash .local/ceph/reset.sh
```

If you need to inspect a failed attempt before resetting:

```bash
ps auxww | grep '[q]emu-nbd'
lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS
```

The initializer does not recover partial virtual-OSD state or reuse an existing backing file. If it fails or is interrupted, run `reset.sh` before retrying. If you need to inspect a failed run first, check for leftover `qemu-nbd` processes:

```bash
ps auxww | grep '[q]emu-nbd'
```

Only disconnect a duplicate process whose command references this repository's file:

```text
.local/ceph/state/osd.raw
```

Do not disconnect the device that `lsblk` shows as containing `ceph_bluestore`; that is the active OSD device.

Disconnect a confirmed duplicate with:

```bash
sudo qemu-nbd --disconnect /dev/nbdN
```

Then rerun:

```bash
bash .local/ceph/init.sh
```

### Configuration overrides

The initializer accepts these environment variables:

```bash
# Defaults to the host's routable IPv4 address. Override if needed.
# CEPH_MON_IP=192.168.40.123
CEPH_IMAGE=quay.io/ceph/ceph:v20.2.3
CEPH_POOL=odorobo-blockpool
CEPH_CLIENT=odorobo
CEPH_IMAGE_NAME=dev-disk
CEPH_IMAGE_SIZE=1G
CEPH_OSD_SIZE=10G
```

To use an intentionally disposable real block device instead of the virtual NBD disk:

```bash
CEPH_OSD_DEVICE=/dev/sdb bash .local/ceph/init.sh
```

Do not use a device containing data. Ceph will format it.

The Ceph bootstrap state is system-wide under `/etc/ceph` and `/var/lib/ceph`. Repository-local state is stored as follows:

```text
.local/ceph/state/osd.raw                 sparse virtual OSD disk
.local/ceph/state/osd.device              attached NBD device name
.local/ceph/generated/ceph.conf           generated client configuration
.local/ceph/generated/client.odorobo.key  generated client credential
```

The generated files and virtual disk state are ignored by git.
