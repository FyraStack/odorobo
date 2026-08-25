#!/usr/bin/env bash
# Destructively remove the local Ceph cluster and virtual OSD state.
set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
GENERATED_DIR="$ROOT_DIR/generated"
STATE_DIR="$ROOT_DIR/state"
OSD_IMAGE="$STATE_DIR/osd.raw"

if [[ $EUID -eq 0 ]]; then
  SUDO=()
else
  SUDO=(sudo)
fi

cephadm() { "${SUDO[@]}" cephadm "$@"; }
rbd() { "${SUDO[@]}" rbd "$@"; }
qemu_nbd() { "${SUDO[@]}" qemu-nbd "$@"; }

nbd_attached() {
  local candidate="$1"
  local proc cmdline
  for proc in /proc/[0-9]*; do
    [[ -r "$proc/cmdline" ]] || continue
    cmdline=$(tr '\0' ' ' < "$proc/cmdline" 2>/dev/null || true)
    if [[ "$cmdline" == *qemu-nbd* \
      && "$cmdline" == *"--connect=$candidate"* \
      && "$cmdline" == *"$OSD_IMAGE"* ]]; then
      return 0
    fi
  done
  return 1
}

if ! command -v cephadm >/dev/null 2>&1; then
  echo "cephadm is required. Install cephadm first." >&2
  exit 1
fi

FSID=$(cephadm ls 2>/dev/null | python3 -c \
  'import json, sys; items=json.load(sys.stdin); print(items[0]["fsid"] if items else "")')

# Unmap any host RBD devices before removing the cluster.
rbd device unmap odorobo-blockpool/dev-disk >/dev/null 2>&1 || true
while read -r _ _ _ _ _ device; do
  [[ -n "${device:-}" ]] || continue
  rbd device unmap "$device" >/dev/null 2>&1 || true
done < <(rbd device list 2>/dev/null | tail -n +2)

if [[ -n "$FSID" ]]; then
  echo "Removing Ceph cluster $FSID..."
  cephadm rm-cluster --force --zap-osds --fsid "$FSID"
else
  echo "No active Ceph cluster found."
fi

# Disconnect only NBD devices whose qemu-nbd process references this project's
# backing file. Never disconnect unrelated NBD devices.
while read -r device; do
  [[ -n "$device" ]] || continue
  if nbd_attached "$device"; then
    echo "Disconnecting $device..."
    qemu_nbd --disconnect "$device" >/dev/null 2>&1 || true
  fi
done < <(ps -eo args= | sed -n 's/.*--connect=\(\/dev\/nbd[0-9][0-9]*\).*/\1/p' | sort -u)

# Remove the virtual OSD image only after the cluster and NBD device are gone.
if [[ -f "$OSD_IMAGE" ]]; then
  echo "Clearing virtual OSD image..."
  OSD_BYTES=$("${SUDO[@]}" stat -c '%s' "$OSD_IMAGE")
  "${SUDO[@]}" dd if=/dev/zero of="$OSD_IMAGE" bs=1M count=16 conv=notrunc status=none || true
  if (( OSD_BYTES >= 33554432 )); then
    "${SUDO[@]}" dd if=/dev/zero of="$OSD_IMAGE" bs=1M seek=$(( (OSD_BYTES / 1048576) - 16 )) count=16 conv=notrunc status=none || true
  fi
fi

rm -rf "$GENERATED_DIR" "$STATE_DIR"
"${SUDO[@]}" rm -rf /etc/ceph/*

echo "Local Ceph cluster, credentials, and virtual OSD state removed."
