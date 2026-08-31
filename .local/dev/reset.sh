#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$ROOT_DIR"

if command -v podman >/dev/null 2>&1 && podman compose version >/dev/null 2>&1; then
  COMPOSE=(podman compose)
elif command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  COMPOSE=(docker compose)
else
  echo "Docker Compose or Podman Compose is required." >&2
  exit 1
fi

"${COMPOSE[@]}" down --remove-orphans --volumes
rm -rf ceph/generated ceph/state

echo "Local Ceph container, credentials, and state removed."
