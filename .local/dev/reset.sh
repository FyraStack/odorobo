#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$ROOT_DIR"

if [[ $EUID -eq 0 ]]; then
  SUDO=()
elif command -v sudo >/dev/null 2>&1; then
  SUDO=(sudo)
else
  echo "reset.sh must run as root or have sudo available to remove Ceph-owned state." >&2
  exit 1
fi

if command -v podman >/dev/null 2>&1 && podman compose version >/dev/null 2>&1; then
  COMPOSE=(podman compose)
elif command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  COMPOSE=(docker compose)
else
  echo "Docker Compose or Podman Compose is required." >&2
  exit 1
fi

"${COMPOSE[@]}" down --remove-orphans --volumes || true
"${SUDO[@]}" rm -rf ceph/generated ceph/state

echo "Local Ceph container, credentials, and state removed."
