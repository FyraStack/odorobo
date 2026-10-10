#!/usr/bin/env bash
# Safe deployment-contract checks: shell syntax and rendered Compose config only.
# This deliberately does not build/start privileged Ceph containers or change
# host kernel/module state.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
COMPOSE_FILE="$SCRIPT_DIR/compose.yml"

for script in "$SCRIPT_DIR/init.sh" "$SCRIPT_DIR/test.sh" "$SCRIPT_DIR/validate-deployment.sh" "$SCRIPT_DIR/ceph/entrypoint.sh" "$SCRIPT_DIR/ceph/healthcheck.sh"; do
  bash -n "$script"
done

if command -v podman >/dev/null 2>&1 && podman compose version >/dev/null 2>&1; then
  COMPOSE=(podman compose)
elif command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  COMPOSE=(docker compose)
else
  echo "Docker Compose or Podman Compose is required for deployment validation." >&2
  exit 1
fi

rendered=$(ODOROBO_ETCD_ENDPOINTS=http://etcd:2379 ODOROBO_HOSTNAME=odorobo "${COMPOSE[@]}" -f "$COMPOSE_FILE" config)
require_text() {
  local output=$1
  local expected=$2
  if ! grep -Fq -- "$expected" <<<"$output"; then
    echo "Rendered Compose config is missing: $expected" >&2
    exit 1
  fi
}

require_text "$rendered" "image: quay.io/coreos/etcd:v3.5.17"
if ! grep -Fq 'etcd_data:/etcd-data' <<<"$rendered" && { ! grep -Fq 'source: etcd_data' <<<"$rendered" || ! grep -Fq 'target: /etcd-data' <<<"$rendered"; }; then
  echo "Rendered Compose config does not persist etcd_data at /etcd-data." >&2
  exit 1
fi
require_text "$rendered" "ODOROBO_ETCD_ENDPOINTS: http://etcd:2379"
require_text "$rendered" "ODOROBO_HOSTNAME: odorobo"
require_text "$(<"$SCRIPT_DIR/init.sh")" "http://127.0.0.1:3000/health"
deadline_guards=$(grep -Fc '(( remaining > 0 )) || break' "$SCRIPT_DIR/init.sh" || true)
if [[ "$deadline_guards" != 2 ]]; then
  echo "Both elapsed-deadline loops must guard against a zero timeout after deadline rollover." >&2
  exit 1
fi
health_dependencies=$(grep -Fc "condition: service_healthy" <<<"$rendered" || true)
if [[ "$health_dependencies" != 2 ]]; then
  echo "Expected Odorobo to wait for healthy Ceph and etcd (found $health_dependencies healthy dependencies)." >&2
  exit 1
fi

external_rendered=$(ODOROBO_ETCD_ENDPOINTS=http://etcd.shared.example:2379 ODOROBO_HOSTNAME=odorobo "${COMPOSE[@]}" -f "$COMPOSE_FILE" config)
require_text "$external_rendered" "ODOROBO_ETCD_ENDPOINTS: http://etcd.shared.example:2379"
require_text "$external_rendered" "ODOROBO_HOSTNAME: odorobo"

hostname_rendered=$(ODOROBO_ETCD_ENDPOINTS=http://etcd:2379 ODOROBO_HOSTNAME=dev-node-2 "${COMPOSE[@]}" -f "$COMPOSE_FILE" config)
require_text "$hostname_rendered" "ODOROBO_ETCD_ENDPOINTS: http://etcd:2379"
require_text "$hostname_rendered" "ODOROBO_HOSTNAME: dev-node-2"

require_text "$(<"$SCRIPT_DIR/ceph/entrypoint.sh")" "rm -f /generated/.odorobo-ceph-ready"
require_text "$(<"$SCRIPT_DIR/ceph/entrypoint.sh")" "touch /generated/.odorobo-ceph-ready"
require_text "$(<"$SCRIPT_DIR/ceph/healthcheck.sh")" 'info "$CEPH_POOL/$CEPH_IMAGE_NAME"'

if ! grep -Fq 'protobuf-compiler' "$SCRIPT_DIR/odorobo/Containerfile"; then
  echo "The development Containerfile must install protobuf-compiler for etcd-client." >&2
  exit 1
fi

# Exercise startup readiness with fake engine commands. These cases do not run
# Compose, launch containers, invoke losetup on the host, or alter host state.
mock_dir=$(mktemp -d)
trap 'rm -rf "$mock_dir"' EXIT
cat >"$mock_dir/podman" <<'MOCK_PODMAN'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == compose ]]; then
  case ${2:-} in
    version|build) exit 0 ;;
    up)
      if [[ ${MOCK_BLOCK_ODOROBO_UP:-0} == 1 && " $* " == *" odorobo "* ]]; then
        sleep 30
      fi
      exit 0
      ;;
    logs) echo "mock logs"; exit 0 ;;
  esac
fi
case ${1:-} in
  inspect)
    container=${!#}
    if [[ "$container" == odorobo-ceph || "$container" == odorobo-etcd ]]; then
      echo "running healthy"
    elif [[ "$container" == odorobo ]]; then
      echo "${MOCK_ODOROBO_STATE:-running}"
    else
      exit 2
    fi
    ;;
  exec)
    sleep "${MOCK_PROBE_DELAY:-0}"
    [[ ${MOCK_PROBE_SUCCESS:-0} == 1 ]]
    ;;
  *) exit 2 ;;
esac
MOCK_PODMAN
cat >"$mock_dir/losetup" <<'MOCK_LOSETUP'
#!/usr/bin/env bash
[[ ${1:-} == -f ]] && printf '/dev/loop-test\n'
MOCK_LOSETUP
chmod +x "$mock_dir/podman" "$mock_dir/losetup"

run_mock_init() {
  local log_file=$1
  shift
  local start end elapsed
  start=$(date +%s)
  if env PATH="$mock_dir:$PATH" "$@" bash "$SCRIPT_DIR/init.sh" >"$log_file" 2>&1; then
    end=$(date +%s)
    printf 'Mocked init unexpectedly succeeded (%s seconds).\n' "$((end - start))" >&2
    cat "$log_file" >&2
    exit 1
  else
    end=$(date +%s)
  fi
  elapsed=$((end - start))
  if (( elapsed > 4 )); then
    printf 'Mocked init exceeded its readiness deadline (%s seconds).\n' "$elapsed" >&2
    cat "$log_file" >&2
    exit 1
  fi
  printf '%s\n' "$elapsed"
}

slow_probe_elapsed=$(run_mock_init "$mock_dir/slow-probe.log" MOCK_PROBE_DELAY=10 ODOROBO_STARTUP_TIMEOUT=2)
if ! grep -Fq 'did not become ready within 2 seconds' "$mock_dir/slow-probe.log"; then
  echo "Slow mocked readiness probe did not produce the startup-timeout diagnostic." >&2
  cat "$mock_dir/slow-probe.log" >&2
  exit 1
fi
exit_elapsed=$(run_mock_init "$mock_dir/exit.log" MOCK_ODOROBO_STATE=exited ODOROBO_STARTUP_TIMEOUT=60)
if ! grep -Fq 'stopped before /health became ready' "$mock_dir/exit.log"; then
  echo "Mocked Odorobo exit was not detected by init.sh." >&2
  cat "$mock_dir/exit.log" >&2
  exit 1
fi
blocked_up_elapsed=$(run_mock_init "$mock_dir/blocked-up.log" MOCK_BLOCK_ODOROBO_UP=1 ODOROBO_STARTUP_TIMEOUT=1)
if ! grep -Fq 'Compose up timed out after 1 seconds for: odorobo' "$mock_dir/blocked-up.log" || ! grep -Fq 'mock logs' "$mock_dir/blocked-up.log"; then
  echo "A blocked Compose dependency wait did not time out with recent logs." >&2
  cat "$mock_dir/blocked-up.log" >&2
  exit 1
fi
printf 'Mocked startup regressions passed (slow probe: %ss, early exit: %ss, blocked Compose up: %ss).\n' "$slow_probe_elapsed" "$exit_elapsed" "$blocked_up_elapsed"

printf 'Deployment validation passed (Compose rendered; no containers or host state changed).\n'
