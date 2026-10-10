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

ODOROBO_STARTUP_TIMEOUT=${ODOROBO_STARTUP_TIMEOUT:-1800}
if [[ ! "$ODOROBO_STARTUP_TIMEOUT" =~ ^[1-9][0-9]*$ ]]; then
  echo "ODOROBO_STARTUP_TIMEOUT must be a positive number of seconds." >&2
  exit 1
fi

if [[ -z "$(losetup -f 2>/dev/null)" ]]; then
  echo "No free loop device is available. Run: sudo modprobe loop" >&2
  exit 1
fi

show_recent_logs() {
  local service=$1
  local lines=$2
  local budget_seconds=$3
  local log_timeout=$((budget_seconds < 5 ? budget_seconds : 5))
  (( log_timeout > 0 )) || return 0
  timeout "$log_timeout" "${COMPOSE[@]}" logs --tail="$lines" "$service" >&2 || true
}

run_compose_up_bounded() {
  local budget_seconds=$1
  shift
  local status

  if timeout --kill-after=1 "${budget_seconds}s" "${COMPOSE[@]}" up -d "$@"; then
    return 0
  else
    status=$?
  fi

  if (( status == 124 || status == 137 )); then
    echo "Compose up timed out after $budget_seconds seconds for: $*" >&2
  else
    echo "Compose up failed (status $status) for: $*" >&2
  fi
  timeout 5 "${COMPOSE[@]}" logs --tail=200 "$@" >&2 || true
  return 1
}

wait_for_healthy() {
  local container_name=$1
  local service=$2
  local label=$3
  local container_state
  local attempt=0
  local deadline=$((SECONDS + 120))
  local remaining
  local command_timeout

  echo "Waiting for $label to become healthy..."
  while (( SECONDS < deadline )); do
    attempt=$((attempt + 1))
    remaining=$((deadline - SECONDS))
    (( remaining > 0 )) || break
    command_timeout=$((remaining < 5 ? remaining : 5))
    # Inspect the engine directly; the external podman-compose provider's `ps`
    # command is not a reliable readiness API.
    container_state=$(timeout "$command_timeout" "${ENGINE[@]}" inspect --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' "$container_name" 2>/dev/null || true)
    if [[ "$container_state" == running\ healthy* ]]; then
      echo "$label is healthy."
      return 0
    elif [[ "$container_state" == running\ unhealthy* || ( "$container_state" != running* && -n "$container_state" ) ]]; then
      echo "$label failed before becoming healthy (state: '$container_state'). Recent logs:" >&2
      remaining=$((deadline - SECONDS))
      show_recent_logs "$service" 200 "$remaining"
      return 1
    fi

    if (( attempt % 10 == 0 )); then
      echo "$label is still not ready (state: '${container_state:-not created}'); recent logs:" >&2
      remaining=$((deadline - SECONDS))
      show_recent_logs "$service" 40 "$remaining"
    fi
    remaining=$((deadline - SECONDS))
    if (( remaining > 0 )); then
      sleep "$((remaining < 2 ? remaining : 2))"
    fi
  done

  echo "$label did not become healthy within 120 seconds (state: '${container_state:-not created}'). Recent logs are available via Compose logs." >&2
  return 1
}

wait_for_odorobo() {
  local deadline=$1
  local container_state
  local attempt=0
  local remaining
  local command_timeout

  echo "Waiting up to $ODOROBO_STARTUP_TIMEOUT seconds for Odorobo's /health endpoint..."
  while (( SECONDS < deadline )); do
    attempt=$((attempt + 1))
    remaining=$((deadline - SECONDS))
    (( remaining > 0 )) || break
    command_timeout=$((remaining < 5 ? remaining : 5))
    container_state=$(timeout "$command_timeout" "${ENGINE[@]}" inspect --format '{{.State.Status}}' odorobo 2>/dev/null || true)
    if [[ "$container_state" == running* ]]; then
      remaining=$((deadline - SECONDS))
      if (( remaining > 0 )); then
        command_timeout=$((remaining < 5 ? remaining : 5))
        if timeout "$command_timeout" "${ENGINE[@]}" exec odorobo curl --fail --silent http://127.0.0.1:3000/health >/dev/null 2>&1; then
          echo "Odorobo is ready."
          return 0
        fi
      fi
    elif [[ "$container_state" == exited* || "$container_state" == dead* || "$container_state" == removing* ]]; then
      echo "Odorobo stopped before /health became ready (state: '$container_state'). Recent logs:" >&2
      show_recent_logs odorobo 200 5
      return 1
    fi

    if (( attempt % 10 == 0 )); then
      echo "Odorobo is still starting (state: '${container_state:-not created}'); recent logs:" >&2
      remaining=$((deadline - SECONDS))
      show_recent_logs odorobo 40 "$remaining"
    fi
    remaining=$((deadline - SECONDS))
    if (( remaining > 0 )); then
      sleep "$((remaining < 2 ? remaining : 2))"
    fi
  done

  echo "Odorobo /health did not become ready within $ODOROBO_STARTUP_TIMEOUT seconds. Recent logs:" >&2
  show_recent_logs odorobo 200 5
  return 1
}

# Keep image compilation outside startup deadlines: the first release build can
# take many minutes. All container creation/wait phases themselves are bounded.
"${COMPOSE[@]}" build ceph odorobo

# Start Ceph and the local persistent etcd store independently so bootstrap
# failures are reported directly rather than hidden behind Compose dependency
# waits. Odorobo is started only after both are healthy.
run_compose_up_bounded 120 ceph etcd
wait_for_healthy odorobo-ceph ceph "Ceph"
wait_for_healthy odorobo-etcd etcd "etcd"

# Compose providers can block while resolving service_healthy dependencies.
# Include that wait and the agent health probe in one startup deadline.
ODOROBO_DEADLINE=$((SECONDS + ODOROBO_STARTUP_TIMEOUT))
remaining=$((ODOROBO_DEADLINE - SECONDS))
if (( remaining <= 0 )); then
  echo "Odorobo startup deadline expired before Compose could start the service." >&2
  show_recent_logs odorobo 200 5
  exit 1
fi
run_compose_up_bounded "$remaining" odorobo
wait_for_odorobo "$ODOROBO_DEADLINE"

cat <<EOF

Ceph, etcd, and Odorobo are ready.
Credentials live in the  ceph_creds named volume, mounted at /generated
in both containers.

For tools running inside the Odorobo container:
  export CEPH_CONFIG=/generated/ceph.conf
  export CEPH_ID=${CEPH_CLIENT:-odorobo}
  export CEPH_KEYFILE=/generated/client.${CEPH_CLIENT:-odorobo}.key
  export CEPH_CLUSTER=ceph
EOF
