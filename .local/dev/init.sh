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

mkdir -p ceph/generated ceph/state/{etc-ceph,lib-ceph,log-ceph,run-ceph,odorobo-ceph}
"${COMPOSE[@]}" up --build -d ceph odorobo
"${COMPOSE[@]}" exec -T ceph ceph -s

cat <<EOF

Ceph is ready.
Config: $ROOT_DIR/ceph/generated/ceph.conf
Key: $ROOT_DIR/ceph/generated/client.${CEPH_CLIENT:-odorobo}.key

Generated credentials (for tools running inside the Odorobo container):
  export CEPH_CONFIG=$ROOT_DIR/ceph/generated/ceph.conf
  export CEPH_ID=${CEPH_CLIENT:-odorobo}
  export CEPH_KEYFILE=$ROOT_DIR/ceph/generated/client.${CEPH_CLIENT:-odorobo}.key
  export CEPH_CLUSTER=ceph
EOF
