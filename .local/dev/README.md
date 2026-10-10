# Local Ceph and Odorobo with Compose

This directory runs the local development stack in containers:

- `etcd` provides the persistent local cluster-state store.
- `ceph` provides a single-node Ceph cluster and a file-backed OSD.
- `odorobo` runs the agent, sharing Ceph's network and PID namespaces and the host's `/dev`.

Odorobo must run in the container for the `rbd://` storage path. It invokes `rbd device map` using the generated Ceph credentials, which creates a kernel block device, and then passes that device to Cloud Hypervisor. The container provides the credential files, the privileged device access, and the shared namespaces that a host-side process would have to replicate.

This is intended for Linux development with a rootful container engine. The stack uses privileged containers because kernel RBD mapping, Cloud Hypervisor, networking, and Ceph's daemon management require host kernel access.

## Prerequisites

Install Podman with a working Compose provider. Docker Compose v2 is supported as a fallback.

For Fedora, the host needs the container engine and kernel modules:

```bash
sudo dnf install -y podman podman-compose kmod
sudo modprobe rbd loop
sudo losetup -f
```

`podman compose` prefers the `docker-compose` plugin when it is installed, and that provider talks to Podman's API socket, which must be running (`systemctl --user enable --now podman.socket` for rootless, `sudo systemctl enable --now podman.socket` for rootful). If you hit `failed to connect to the docker API at unix:///run/user/<uid>/podman/podman.sock`, either enable that socket or select the standalone tool as the provider: `compose_providers = ["podman-compose"]` in `~/.config/containers/containers.conf` (or `export PODMAN_COMPOSE_PROVIDER=podman-compose`).

The stack must run on a **rootful** engine. Rootless Podman cannot work: the Ceph container attaches the OSD file through a host loop device, and the kernel's loop driver requires `CAP_SYS_ADMIN` in the initial user namespace, which a rootless container never has (even `privileged` + a `/dev` bind mount do not help). Run everything rootful, e.g. `sudo bash .local/dev/init.sh`, and prefix the `podman compose` commands below with `sudo` accordingly.

Two more host requirements:

- **SELinux**: the Ceph entrypoint `chown`s its state directories and drops the daemons to the `ceph` user. On an enforcing SELinux host, container processes (`container_t`) are not allowed to chown host-created files, so bootstrap fails with `Permission denied`. Set the host to permissive (`sudo setenforce 0`, persist with `SELINUX=permissive` in `/etc/selinux/config`) or write a policy for the container domain.
- **Ceph udev rule (recommended)**: the agent prefers the stable `/dev/rbd/<pool>/<image>` device path, which the host's `udevd` creates from the rbd kernel uevent using Ceph's `50-rbd.rules` and `ceph-rbdnamer` (shipped with the Ceph package). Install them on the host (e.g. `sudo dnf install ceph-common` or copy them from the Ceph container) for stable names; without them the agent falls back to the kernel device name (e.g. `/dev/rbd0`). Note that `rbd device map` with udev enabled (the default) hangs inside containers: the CLI waits for a udev event in its own network namespace, but rbd devices are created in the host's namespace. The agent and `test.sh` therefore map with `--options noudev`.

The Ceph image is based on the official `quay.io/ceph/ceph` image and starts the MON and OSD daemons itself; it intentionally does not start MGR because the MGR's optional Python modules require host udev/system services unavailable in this container. It does not use `cephadm`, nested Podman, or systemd. The OSD uses a persistent raw file attached through a host loop device, initialized directly with `ceph-osd` rather than `ceph-volume`.

## Usage

Initialize Ceph and start Odorobo:

```bash
sudo bash .local/dev/init.sh
```

This builds the development images, starts Ceph and etcd, waits for both health checks, and then starts Odorobo with manager mode enabled. Ceph is not considered healthy until bootstrap and RBD provisioning finish, the generated client credentials authenticate, and the configured `odorobo-blockpool/dev-disk` image is accessible. Image builds are outside the startup timeout, but Odorobo's initial release-mode Cargo compilation happens during `cargo run` and is included. Once Ceph and etcd are healthy, `ODOROBO_STARTUP_TIMEOUT` (30 minutes by default) bounds Compose's agent/dependency startup wait—including that compilation—and the agent's `/health` readiness probe. Set it to a different number of seconds if needed. Startup failures report recent service logs. It prefers `podman compose` and falls back to `docker compose` when Podman Compose is unavailable.

Start and stop the complete stack without deleting data. Compose waits for healthy etcd and Ceph before starting Odorobo; after a destructive reset, run `init.sh` again to recreate and provision the stack:

```bash
sudo podman compose -f .local/dev/compose.yml up -d etcd ceph odorobo
sudo podman compose -f .local/dev/compose.yml stop odorobo ceph etcd
```

Destructively remove the containers and all local Ceph and etcd state (the named volumes are removed by `--volumes`).

```bash
sudo podman compose -f .local/dev/compose.yml down --remove-orphans --volumes
```

Useful direct commands:

```bash
sudo podman compose -f .local/dev/compose.yml ps
sudo podman compose -f .local/dev/compose.yml logs -f etcd ceph odorobo
sudo podman compose -f .local/dev/compose.yml exec ceph ceph -s
sudo podman compose -f .local/dev/compose.yml exec -it odorobo sh
```

Because `odorobo` uses Ceph's network namespace, the generated Ceph config intentionally uses `127.0.0.1` for the monitor. The application and monitor share that namespace. The local etcd service is reachable there by the Compose DNS name `etcd`; Compose sets `ODOROBO_ETCD_ENDPOINTS=http://etcd:2379` by default. It also sets `ODOROBO_HOSTNAME=odorobo` by default, giving the application a stable identity across container recreation without relying on container hostname support in the shared-network mode. Override it with a stable, unique node ID when multiple Odorobo nodes use one shared etcd store; every node in that store must have its own identity, kept unchanged across restarts. The etcd data is stored in the `etcd_data` named volume and survives container recreation. To use an existing shared etcd instead, export both variables and pass them through `sudo` when running the rootful stack (the local etcd service remains part of this development stack):

```bash
export ODOROBO_ETCD_ENDPOINTS=http://etcd.example.net:2379
export ODOROBO_HOSTNAME=dev-node-1
sudo --preserve-env=ODOROBO_ETCD_ENDPOINTS,ODOROBO_HOSTNAME bash .local/dev/init.sh
```

## Application development

The repository is mounted at `/workspace` in the Odorobo container. Rebuild and restart the application after source changes:

```bash
podman compose -f .local/dev/compose.yml build odorobo
podman compose -f .local/dev/compose.yml up -d odorobo
podman compose -f .local/dev/compose.yml logs -f odorobo
```

By default, the container limits Cargo to two concurrent build jobs to reduce CPU and memory pressure during the initial release build. Override it when starting the stack, for example `CARGO_BUILD_JOBS=4 bash .local/dev/init.sh`.

The agent runs as:

```text
cargo run --release -p odorobo -- --manager-enabled true
```

Its runtime directory is container-local (`/run/odorobo` in the `odorobo` container); Cloud Hypervisor processes and RBD devices are visible in the same namespaces as the agent.

## Verify the image

Run Ceph commands inside the Ceph container:

```bash
podman compose -f .local/dev/compose.yml exec ceph rbd \
  --conf=/etc/ceph/ceph.conf --id=odorobo \
  --keyfile=/var/lib/odorobo-ceph/client.odorobo.key \
  ls --pool odorobo-blockpool
```

Expected output includes `dev-disk`. A one-node cluster may report `HEALTH_WARN`; reduced redundancy, a single monitor, and no running MGR are expected for local development.

Do not map the image from the host. To test the exact application path, use an Odorobo manifest with `rbd://odorobo-blockpool/dev-disk`; the `odorobo` service will execute `rbd device map` and pass the resulting device to Cloud Hypervisor.

## Configuration

The following environment variables can be set before `init.sh`; all except `ODOROBO_STARTUP_TIMEOUT` are passed through Compose:

```bash
CEPH_IMAGE=quay.io/ceph/ceph:v20.2.3
CEPH_MON_IP=127.0.0.1
CEPH_POOL=odorobo-blockpool
CEPH_CLIENT=odorobo
CEPH_IMAGE_NAME=dev-disk
CEPH_IMAGE_SIZE=1G
CEPH_OSD_SIZE=10G
ODOROBO_ETCD_ENDPOINTS=http://etcd:2379
ODOROBO_HOSTNAME=odorobo # stable; set a unique node ID for a shared etcd store
ODOROBO_STARTUP_TIMEOUT=1800 # seconds; includes initial Cargo compilation, excludes image builds
```

`CEPH_MON_IP` should remain `127.0.0.1` with the provided Compose topology. If you change the network topology, it must be an address reachable from both services.

## End-to-end test

`test.sh` boots a guest OS via rust-hypervisor-firmware (UEFI, no
direct kernel boot) from the `odorobo-blockpool/dev-disk` RBD image, then
hands the serial console to you so you can log in and verify by hand:

```bash
sudo bash .local/dev/test.sh
```

The pinned default is Fedora 44 Cloud Base (login `fedora`/`fedora`). Note
that rust-hypervisor-firmware 0.5.0 currently cannot load the GRUB from newer
distro images (upstream
[cloud-hypervisor/rust-hypervisor-firmware#412](https://github.com/cloud-hypervisor/rust-hypervisor-firmware/issues/412),
tracked in HACKING.md): the firmware finds the EFI partition and bootloader on
the RBD disk but fails with `Error loading executable: FileError`. The full
path to the login prompt is verified with Ubuntu 22.04 (jammy) — the image the
firmware was developed against — via the `IMAGE_URL`/`IMAGE_SHA256` overrides
(the asset is the qcow2 generic image converted to raw with `qemu-img
convert -f qcow2 -O raw`, then xz-compressed):

```bash
IMAGE_URL="https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64-raw.img.xz" \
IMAGE_SHA256=28bd1513fb0903e37a483279d83fd34b79ddf9785c3d033df99067e14fc1abdc \
  sudo bash .local/dev/test.sh
```

(For the jammy run, log in as `ubuntu`/`ubuntu` if the image allows password
login; the login prompt itself proves the OS booted.)

It fetches and caches the firmware and the image into `test-assets/`, writes
the image into the pool (host-side `dd` into the mapped RBD device), creates
the VM through the agent with a firmware boot, watches the boot stages, and
cleans up after you detach. See TEST.md for details and overrides.

## Layout

- `init.sh` — builds and starts the stack, waits for etcd, Ceph, and Odorobo health, and reports startup failures with logs.
- `test.sh` — the end-to-end firmware-boot test (run from the host; see TEST.md); it preflights etcd, Ceph, and agent health.
- `validate-deployment.sh` — safe shell-syntax and Compose-rendering regression checks; it does not start containers or change host state.
- `compose.yml` — etcd, Ceph, and Odorobo services, shared namespaces, health checks, privilege, and mounts.
- `ceph/Containerfile` — pinned Ceph image.
- `ceph/entrypoint.sh` — direct MON bootstrap, filesystem-backed OSD initialization, pool/client/image provisioning, and daemon lifecycle.
- `odorobo/Containerfile` — runnable Odorobo development image.
- `test-assets/` — firmware and guest image downloaded by `test.sh`; ignored by git.
- Ceph state (config, daemons, logs, file-backed OSD) and generated credentials live in named volumes (`ceph_etc`, `ceph_lib`, `ceph_log`, `ceph_run`, `ceph_osd`, `ceph_creds`); `ceph_creds` is shared read-only with Odorobo. Etcd data lives in `etcd_data`. All are removed by `compose down --volumes`, which resets both the local Ceph cluster and durable cluster state.

This setup is intentionally not production-ready: it has one monitor, one OSD, no redundancy, and privileged containers.
