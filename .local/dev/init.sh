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

if [[ -z "$(losetup -f 2>/dev/null)" ]]; then
  echo "No free loop device is available. Run: sudo modprobe loop" >&2
  exit 1
fi

"${COMPOSE[@]}" up --build -d ceph etcd

for service in etcd ceph; do
  container="odorobo-$service"
  echo "Waiting for $service to become healthy..."
  for attempt in {1..60}; do
    state=$(timeout 5 "${ENGINE[@]}" inspect --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' "$container" 2>/dev/null || true)
    if [[ "$state" == running\ healthy* ]]; then
      break
    elif [[ "$state" == exited* || "$state" == dead* || "$state" == running\ unhealthy* ]]; then
      echo "$service failed to become healthy (state: '$state'). Recent logs:" >&2
      "${COMPOSE[@]}" logs --tail=200 "$service" >&2 || true
      exit 1
    fi
    if (( attempt % 10 == 0 )); then
      echo "$service is still starting; recent logs:" >&2
      "${COMPOSE[@]}" logs --tail=40 "$service" >&2 || true
    fi
    sleep 2
  done
  state=$(timeout 5 "${ENGINE[@]}" inspect --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' "$container" 2>/dev/null || true)
  if [[ "$state" != running\ healthy* ]]; then
    echo "$service did not become healthy within 120 seconds. Recent logs:" >&2
    "${COMPOSE[@]}" logs --tail=200 "$service" >&2 || true
    exit 1
  fi
done

"${COMPOSE[@]}" up -d odorobo

echo "etcd, Ceph, and Odorobo are ready."
echo "etcd data is stored in the etcd_data volume; compose down --volumes resets it."
