# Local Ceph and Odorobo with Compose

This directory runs the local development stack in containers:

- `ceph` provides a single-node Ceph cluster and a file-backed raw OSD.
- `odorobo` runs the agent in the same network, PID, and device namespaces as Ceph.

Odorobo must run in the container for the `rbd://` storage path. It invokes `rbd device map`, which creates a kernel block device, and then passes that device to Cloud Hypervisor. A host-side process would not see the container's `/dev/rbd*` device or have the required device and privilege context.

This is intended for Linux development with a rootful container engine. The stack uses privileged containers because kernel RBD mapping, Cloud Hypervisor, networking, and Ceph's daemon management require host kernel access.

## Prerequisites

Install Podman with a working Compose provider. Docker Compose v2 is supported as a fallback.

For Fedora, the host needs the container engine and kernel modules:

```bash
sudo dnf install -y podman podman-compose kmod
sudo modprobe rbd
```

The container image installs `ceph-common`, Rust tooling, and Cloud Hypervisor tooling. The host does not need `cephadm`, `ceph-common`, `qemu-nbd`, or systemd Ceph units.

## Usage

Initialize Ceph and start Odorobo:

```bash
bash .local/dev/init.sh
```

This builds both images, starts both services, provisions the `odorobo-blockpool/dev-disk` RBD image, and starts Odorobo with manager mode enabled.

Start and stop the complete stack without deleting data:

```bash
bash .local/dev/start.sh
bash .local/dev/stop.sh
```

Destructively remove the containers and all local Ceph state:

```bash
bash .local/dev/reset.sh
```

The scripts prefer `podman compose` and fall back to `docker compose` when Podman Compose is unavailable.

Useful direct commands:

```bash
podman compose -f .local/dev/compose.yml ps
podman compose -f .local/dev/compose.yml logs -f ceph odorobo
podman compose -f .local/dev/compose.yml exec ceph ceph -s
```

Because `odorobo` uses Ceph's network namespace, the generated Ceph config intentionally uses `127.0.0.1` for the monitor. The application and monitor share that namespace.

## Application development

The repository is mounted at `/workspace` in the Odorobo container. Rebuild and restart the application after source changes:

```bash
podman compose -f .local/dev/compose.yml build odorobo
podman compose -f .local/dev/compose.yml up -d odorobo
podman compose -f .local/dev/compose.yml logs -f odorobo
```

The agent runs as:

```text
cargo run --release -p odorobo -- --manager-enabled
```

Its runtime directory is shared through `/run/odorobo`, and Cloud Hypervisor processes and RBD devices are visible in the same namespaces as the agent.

## Verify the image

Run Ceph commands inside the Ceph container:

```bash
podman compose -f .local/dev/compose.yml exec ceph rbd \
  --conf=/etc/ceph/ceph.conf --id=odorobo \
  --keyfile=/var/lib/odorobo-ceph/client.odorobo.key \
  ls --pool odorobo-blockpool
```

Expected output includes `dev-disk`. A one-node cluster may report `HEALTH_WARN`; reduced redundancy and a single monitor are expected for local development.

Do not map the image from the host. To test the exact application path, use an Odorobo manifest with `rbd://odorobo-blockpool/dev-disk`; the `odorobo` service will execute `rbd device map` and pass the resulting device to Cloud Hypervisor.

## Configuration

The following environment variables can be set before `init.sh` or passed through Compose:

```bash
CEPH_IMAGE=quay.io/ceph/ceph:v20.2.3
CEPH_MON_IP=127.0.0.1
CEPH_POOL=odorobo-blockpool
CEPH_CLIENT=odorobo
CEPH_IMAGE_NAME=dev-disk
CEPH_IMAGE_SIZE=1G
CEPH_OSD_SIZE=10G
```

`CEPH_MON_IP` should remain `127.0.0.1` with the provided Compose topology. If you change the network topology, it must be an address reachable from both services.

## Layout

- `compose.yml` — Ceph and Odorobo services, shared namespaces, privilege, mounts, and ports.
- `ceph/Containerfile` — pinned Ceph image.
- `ceph/entrypoint.sh` — bootstrap, OSD, pool, client, and image provisioning.
- `odorobo/Containerfile` — runnable Odorobo development image.
- `ceph/generated/` — generated Ceph credentials shared read-only with Odorobo; ignored by git.
- `ceph/state/` — Ceph configuration, daemons, logs, and file-backed OSD; ignored by git.

This setup is intentionally not production-ready: it has one monitor, one OSD, no redundancy, and privileged containers.
