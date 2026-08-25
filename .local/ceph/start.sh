#!/usr/bin/env bash
# Start an existing local Ceph cluster without provisioning or deleting data.
set -euo pipefail

if [[ $EUID -eq 0 ]]; then
  SUDO=()
else
  SUDO=(sudo)
fi

cephadm() { "${SUDO[@]}" cephadm "$@"; }
ceph() { "${SUDO[@]}" cephadm shell -- ceph "$@"; }

if ! command -v cephadm >/dev/null 2>&1; then
  echo "cephadm is required. Install cephadm first." >&2
  exit 1
fi

if [[ ! -f /etc/ceph/ceph.conf ]]; then
  echo "No local Ceph cluster exists. Run init.sh first:" >&2
  echo "  bash .local/ceph/init.sh" >&2
  exit 1
fi

FSID=$(cephadm ls 2>/dev/null | python3 -c \
  'import json, sys; items=json.load(sys.stdin); print(items[0]["fsid"] if items else "")')
if [[ -z "$FSID" ]]; then
  echo "Could not determine the local Ceph cluster FSID." >&2
  exit 1
fi

TARGET="ceph-$FSID.target"
echo "Starting Ceph cluster $FSID..."
"${SUDO[@]}" systemctl start "$TARGET"

for _ in {1..30}; do
  if ceph -s >/dev/null 2>&1; then
    echo "Local Ceph cluster is available."
    exit 0
  fi
  sleep 2
done

echo "Ceph did not become available after starting $TARGET." >&2
"${SUDO[@]}" systemctl status "$TARGET" --no-pager >&2 || true
ceph -s >&2 || true
exit 1
