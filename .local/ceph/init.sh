#!/usr/bin/env bash
# Run with: bash .local/ceph/init.sh
set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
GENERATED_DIR="$ROOT_DIR/generated"
CEPH_IMAGE="${CEPH_IMAGE:-quay.io/ceph/ceph:v20.2.3}"
CEPH_OSD_DEVICE="${CEPH_OSD_DEVICE:-}"
CEPH_OSD_SIZE="${CEPH_OSD_SIZE:-10G}"
OSD_STATE_DIR="$ROOT_DIR/state"
OSD_IMAGE="$OSD_STATE_DIR/osd.raw"
if [[ -n "${CEPH_MON_IP:-}" ]]; then
  MON_IP="$CEPH_MON_IP"
else
  MON_IP=$(ip -4 route get 1.1.1.1 2>/dev/null \
    | awk '{for (i=1; i<=NF; i++) if ($i == "src") {print $(i+1); exit}}')
  MON_IP="${MON_IP:-127.0.0.1}"
fi
POOL_NAME="${CEPH_POOL:-odorobo-blockpool}"
CLIENT_NAME="${CEPH_CLIENT:-odorobo}"
IMAGE_NAME="${CEPH_IMAGE_NAME:-dev-disk}"
IMAGE_SIZE="${CEPH_IMAGE_SIZE:-1G}"

if [[ $EUID -eq 0 ]]; then
  SUDO=()
else
  SUDO=(sudo)
fi

cephadm() { "${SUDO[@]}" cephadm "$@"; }
# Run cluster-control commands inside the pinned Ceph container so the CLI
# version and authentication behavior match the daemons.
ceph() { "${SUDO[@]}" cephadm shell -- ceph "$@"; }
rbd() { "${SUDO[@]}" rbd "$@"; }
qemu_img() { "${SUDO[@]}" qemu-img "$@"; }
qemu_nbd() { "${SUDO[@]}" qemu-nbd "$@"; }


mkdir -p "$GENERATED_DIR" "$OSD_STATE_DIR"

if ! command -v podman >/dev/null 2>&1; then
  echo "Podman is required because cephadm runs the Ceph daemons in Podman containers." >&2
  echo "For Fedora: sudo dnf install -y podman" >&2
  exit 1
fi

if ! command -v cephadm >/dev/null 2>&1; then
  echo "cephadm is required. Install it with your distribution package manager." >&2
  echo "For Fedora: sudo dnf install -y cephadm ceph-common" >&2
  exit 1
fi

if ! command -v ceph >/dev/null 2>&1; then
  echo "ceph CLI is required. Install ceph-common or use cephadm shell." >&2
  echo "For Fedora: sudo dnf install -y ceph-common" >&2
  exit 1
fi

if [[ -z "$CEPH_OSD_DEVICE" ]]; then
  for command in qemu-img qemu-nbd; do
    if ! command -v "$command" >/dev/null 2>&1; then
      echo "$command is required for the virtual OSD device." >&2
      echo "For Fedora: sudo dnf install -y qemu-img" >&2
      exit 1
    fi
  done
fi

if [[ ! -f /etc/ceph/ceph.conf ]]; then
  echo "Bootstrapping single-host Ceph with $CEPH_IMAGE..."
  BOOTSTRAP_ARGS=(
    --mon-ip "$MON_IP"
    --single-host-defaults
    --output-dir /etc/ceph
    --skip-monitoring-stack
    --allow-fqdn-hostname
  )
  if [[ "$MON_IP" == "127.0.0.1" || "$MON_IP" == "::1" ]]; then
    BOOTSTRAP_ARGS+=(--skip-mon-network)
  fi
  cephadm --image "$CEPH_IMAGE" bootstrap "${BOOTSTRAP_ARGS[@]}"
fi

if ! ceph -s >/dev/null 2>&1; then
  echo "Ceph did not become available after bootstrap." >&2
  cephadm shell -- ceph -s >&2 || true
  exit 1
fi

OSD_EXISTS=0
if [[ -n "$(ceph osd ls 2>/dev/null)" ]]; then
  OSD_EXISTS=1
fi
VIRTUAL_OSD=0
if [[ "$OSD_EXISTS" -eq 0 && -z "$CEPH_OSD_DEVICE" ]]; then
  VIRTUAL_OSD=1
  "${SUDO[@]}" modprobe nbd max_part=8

  if [[ -f "$OSD_IMAGE" || -f "$OSD_STATE_DIR/osd.device" ]]; then
    echo "Local virtual OSD state already exists." >&2
    echo "Run reset.sh before reinitializing:" >&2
    echo "  bash .local/ceph/reset.sh" >&2
    exit 1
  fi

  echo "Creating a ${CEPH_OSD_SIZE} virtual OSD disk at $OSD_IMAGE..."
  qemu_img create -f raw "$OSD_IMAGE" "$CEPH_OSD_SIZE"

  OSD_DEVICE=""
  echo "Attaching virtual OSD disk..."
  for candidate in /dev/nbd{0..15}; do
    if [[ -b "$candidate" ]] && (( $("${SUDO[@]}" blockdev --getsize64 "$candidate" 2>/dev/null || echo 0) > 0 )); then
      continue
    fi
    attach_log="$OSD_STATE_DIR/qemu-nbd-${candidate##*/}.log"
    echo "Trying qemu-nbd on $candidate..."
    if ! "${SUDO[@]}" qemu-nbd --persistent --fork --verbose \
      --connect="$candidate" --format=raw "$OSD_IMAGE" \
      >"$attach_log" 2>&1; then
      cat "$attach_log" >&2 || true
      continue
    fi
    candidate_size=0
    for _ in {1..60}; do
      "${SUDO[@]}" udevadm settle || true
      candidate_size=$("${SUDO[@]}" blockdev --getsize64 "$candidate" 2>/dev/null || echo 0)
      if (( candidate_size >= 5368709120 )); then
        break
      fi
      sleep 1
    done
    if (( candidate_size >= 5368709120 )); then
      OSD_DEVICE="$candidate"
      break
    fi
    echo "qemu-nbd did not expose a usable block device on $candidate after 60 seconds (size: ${candidate_size} bytes)." >&2
    cat "$attach_log" >&2 || true
    "${SUDO[@]}" qemu-nbd --disconnect "$candidate" >/dev/null 2>&1 || true
  done

  if [[ -z "$OSD_DEVICE" ]]; then
    echo "Could not attach a virtual OSD disk of at least 5 GiB." >&2
    echo "qemu-img info:"
    qemu_img info "$OSD_IMAGE" >&2 || true
    echo "qemu-nbd attach logs:"
    for attach_log in "$OSD_STATE_DIR"/qemu-nbd-*.log; do
      [[ -f "$attach_log" ]] || continue
      echo "--- $attach_log" >&2
      cat "$attach_log" >&2
    done
    exit 1
  fi

  echo "$OSD_DEVICE" > "$OSD_STATE_DIR/osd.device"
  CEPH_OSD_DEVICE="$OSD_DEVICE"
fi

# `ceph osd ls` is deliberately used instead of matching formatted JSON output.
if [[ "$OSD_EXISTS" -eq 0 ]]; then
  if [[ ! -b "$CEPH_OSD_DEVICE" ]]; then
    echo "CEPH_OSD_DEVICE is not a block device: $CEPH_OSD_DEVICE" >&2
    exit 1
  fi

  if [[ "$VIRTUAL_OSD" -eq 1 ]]; then
    echo "Zapping virtual OSD device $CEPH_OSD_DEVICE..."
    "${SUDO[@]}" wipefs --all --force "$CEPH_OSD_DEVICE" >/dev/null 2>&1 || true
    "${SUDO[@]}" pvremove --force --force --yes "$CEPH_OSD_DEVICE" >/dev/null 2>&1 || true
    "${SUDO[@]}" udevadm settle || true
  fi

  echo "Creating an OSD on $CEPH_OSD_DEVICE..."
  if ! timeout --foreground 120 "${SUDO[@]}" cephadm shell -- ceph orch daemon add osd "$(hostname -s):$CEPH_OSD_DEVICE"; then
    echo "cephadm could not create an OSD on $CEPH_OSD_DEVICE." >&2
    ceph orch device ls --wide >&2 || true
    ceph orch ps --daemon-type osd >&2 || true
    exit 1
  fi
fi

# Wait for the OSD to register. The cluster may remain HEALTH_WARN due to
# being a one-node cluster; that is expected.
for _ in {1..60}; do
  if ceph osd stat 2>/dev/null | grep -q '[1-9][0-9]* osds:'; then
    break
  fi
  sleep 2
done

if ! ceph osd stat 2>/dev/null | grep -q '[1-9][0-9]* osds:'; then
  echo "The OSD did not become available." >&2
  ceph orch ps --daemon-type osd >&2 || true
  exit 1
fi

if ! ceph osd pool ls --format=json | grep -qF "\"$POOL_NAME\""; then
  ceph osd pool create "$POOL_NAME" 8
fi
# A one-OSD development cluster needs a replicated pool size of one. Ceph
# requires this explicit opt-in instead of allowing it by default.
ceph config set mon mon_allow_pool_size_one true
ceph config set osd osd_pool_default_size 1
ceph config set osd osd_pool_default_min_size 1
ceph osd pool set "$POOL_NAME" size 1 --yes-i-really-mean-it
ceph osd pool set "$POOL_NAME" min_size 1
rbd pool init "$POOL_NAME"

if ! ceph auth get "client.$CLIENT_NAME" >/dev/null 2>&1; then
  ceph auth get-or-create "client.$CLIENT_NAME" \
    mon "allow r" \
    osd "allow rwx pool=$POOL_NAME" >/dev/null
fi

ceph config generate-minimal-conf > "$GENERATED_DIR/ceph.conf"
ceph auth get-key "client.$CLIENT_NAME" > "$GENERATED_DIR/client.$CLIENT_NAME.key"
chmod 600 "$GENERATED_DIR/client.$CLIENT_NAME.key"

if ! rbd info "$POOL_NAME/$IMAGE_NAME" >/dev/null 2>&1; then
  rbd create "$POOL_NAME/$IMAGE_NAME" --size "$IMAGE_SIZE"
fi

echo "Ceph is ready."
echo "Image: $POOL_NAME/$IMAGE_NAME"
echo "Config: $GENERATED_DIR/ceph.conf"
echo "Key: $GENERATED_DIR/client.$CLIENT_NAME.key"
echo
echo "Export for host-side Odorobo:"
echo "  export CEPH_CONFIG=$GENERATED_DIR/ceph.conf"
echo "  export CEPH_ID=$CLIENT_NAME"
echo "  export CEPH_KEYFILE=$GENERATED_DIR/client.$CLIENT_NAME.key"
