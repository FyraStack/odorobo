#!/usr/bin/env bash
# Stop the local Ceph cluster without deleting its data.
set -euo pipefail

if [[ $EUID -eq 0 ]]; then
  SUDO=()
else
  SUDO=(sudo)
fi

cephadm() { "${SUDO[@]}" cephadm "$@"; }
rbd() { "${SUDO[@]}" rbd "$@"; }

# Unmap the known test image if it is mapped. Ignore the expected error when it
# is already unmapped.
rbd device unmap odorobo-blockpool/dev-disk >/dev/null 2>&1 || true

# Unmap any remaining RBD devices. This matters if a test used another image.
while read -r _ _ _ _ _ device; do
  [[ -n "${device:-}" ]] || continue
  rbd device unmap "$device" >/dev/null 2>&1 || true
done < <(rbd device list 2>/dev/null | tail -n +2)

if ! command -v cephadm >/dev/null 2>&1; then
  echo "cephadm is required. Install cephadm first." >&2
  exit 1
fi

FSID=$(cephadm ls 2>/dev/null | python3 -c \
  'import json, sys; items=json.load(sys.stdin); print(items[0]["fsid"] if items else "")')
if [[ -z "$FSID" ]]; then
  echo "No local Ceph cluster found; nothing to stop."
  exit 0
fi

TARGET="ceph-$FSID.target"
echo "Stopping Ceph cluster $FSID..."
"${SUDO[@]}" systemctl stop "$TARGET"

echo "Local Ceph daemons stopped. Data and virtual OSD state were preserved."
