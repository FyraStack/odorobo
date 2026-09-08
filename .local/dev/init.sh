#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$ROOT_DIR"

if command -v podman >/dev/null 2>&1 && podman compose version >/dev/null 2>&1; then
  COMPOSE=(podman compose)
  ENGINE=(podman)
elif command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  COMPOSE=(docker compose)
  ENGINE=(docker)
else
  echo "Docker Compose or Podman Compose is required." >&2
  exit 1
fi


if ! losetup -f >/dev/null 2>&1; then
  echo "No loop device is available. Run: sudo modprobe loop && sudo losetup -f" >&2
  exit 1
fi

mkdir -p ceph/generated ceph/state/{etc-ceph,lib-ceph,log-ceph,run-ceph,odorobo-ceph}

# Start Ceph independently: `odorobo` depends on its health check, and starting
# both at once hides a Ceph bootstrap failure behind Compose's dependency wait.
"${COMPOSE[@]}" up --build -d ceph

echo "Waiting for Ceph to become healthy..."
for attempt in {1..60}; do
  # Inspect the engine directly; the external podman-compose provider's `ps`
  # command is not a reliable readiness API.
  container_state=$(timeout 5 "${ENGINE[@]}" inspect --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' odorobo-ceph 2>/dev/null || true)
  if [[ "$container_state" == running\ healthy* ]]; then
    break
  elif [[ "$container_state" != running* && -n "$container_state" ]]; then
    echo "Ceph stopped before becoming healthy. Recent logs:" >&2
    "${COMPOSE[@]}" logs --tail=200 ceph >&2 || true
    exit 1
  fi

  if (( attempt % 10 == 0 )); then
    echo "Ceph is still not ready; recent logs:" >&2
    "${COMPOSE[@]}" logs --tail=40 ceph >&2 || true
  fi
  sleep 2
done

if [[ $(timeout 5 "${ENGINE[@]}" inspect --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' odorobo-ceph 2>/dev/null || true) != running\ healthy* ]]; then
  echo "Ceph did not become healthy within 120 seconds. Recent logs:" >&2
  "${COMPOSE[@]}" logs --tail=200 ceph >&2 || true
  exit 1
fi

"${COMPOSE[@]}" up -d odorobo

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
