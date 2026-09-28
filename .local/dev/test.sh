#!/usr/bin/env bash
# End-to-end test: boot a Fedora VM via firmware (rust-hypervisor-firmware)
# from a Ceph RBD disk, then hand the serial console to a human.
#
# Run this from the HOST (rootful container engine required, see README.md):
#
#   sudo bash .local/dev/init.sh     # one-time: build + start the stack
#   sudo bash .local/dev/test.sh     # this test
#
# What it does:
#   1. verifies the stack is up (Ceph healthy, agent healthy)
#   2. fetches the UEFI firmware and the Fedora image into test-assets/
#      (cached; the image is sha256-pinned, the firmware is size/magic-pinned)
#   3. writes the image into the Ceph RBD pool. The rbd control commands run
#      in the container (the generated ceph.conf targets the container's
#      loopback), but the actual write is a plain `dd` from the host into
#      /dev/rbd/<pool>/<image> -- the mapping is a kernel object and /dev is
#      shared, so host-side writes are the exact path a host tool would use.
#      The test unmaps afterwards so the agent performs the mapping itself
#      (the application path under test).
#   4. creates the VM through the agent with a firmware boot (no kernel, no
#      cmdline; the manifest's boot.firmware points at the repo-mounted
#      firmware, which the agent resolves as an absolute container path)
#   5. watches the serial console history for the boot stages (firmware ->
#      EFI/GRUB -> kernel -> login prompt)
#   6. attaches the interactive console (socat) so a human can log in
#      (fedora/fedora) and verify things by hand
#   7. after detach: deletes the VM and unmaps the RBD (KEEP_VM=1 to skip)
#
# Useful overrides (environment):
#   VM_VCPUS=2 VM_MEMORY_GB=2 BOOT_TIMEOUT=240 KEEP_VM=1 SKIP_IMAGE_WRITE=1
#
# SKIP_IMAGE_WRITE=1 skips the RBD write (for fast re-runs after a failed
# boot; the image from the previous run is reused as-is).
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ASSET_DIR="$SCRIPT_DIR/test-assets"

# --- assets (pinned) ---------------------------------------------------------
FIRMWARE_URL="${FIRMWARE_URL:-https://github.com/cloud-hypervisor/rust-hypervisor-firmware/releases/download/0.5.0/hypervisor-fw}"
FIRMWARE_SIZE="${FIRMWARE_SIZE:-135960}"
IMAGE_URL="${IMAGE_URL:-https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-Base-AmazonEC2-44-1.7.x86_64.raw.xz}"
IMAGE_SHA256="${IMAGE_SHA256:-7e4fb73907abdc761d226ddaf3263bdfca62a0b0bfb5f0798545a9981fdd1953}"

# --- VM shape ---------------------------------------------------------------
VM_NAME="${VM_NAME:-fedora-firmware-e2e}"
VM_VCPUS="${VM_VCPUS:-2}"
VM_MEMORY_GB="${VM_MEMORY_GB:-2}"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-240}"
KEEP_VM="${KEEP_VM:-0}"
SKIP_IMAGE_WRITE="${SKIP_IMAGE_WRITE:-0}"

# --- Ceph coordinates (match compose.yml defaults) ---------------------------
CEPH_POOL="${CEPH_POOL:-odorobo-blockpool}"
CEPH_IMAGE_NAME="${CEPH_IMAGE_NAME:-dev-disk}"
CEPH_CLIENT="${CEPH_CLIENT:-odorobo}"

# The repo is mounted at /workspace in the odorobo container; the agent
# requires an absolute firmware path, so point at the mounted copy.
FIRMWARE_PATH_IN_CONTAINER="/workspace/.local/dev/test-assets/hypervisor-fw"

FIRMWARE_FILE="$ASSET_DIR/hypervisor-fw"
IMAGE_FILE="$ASSET_DIR/$(basename "$IMAGE_URL")"

VMID=""
ENGINE=()

log() { printf '\n[test] %s\n' "$*"; }
die() { printf '\n[test] ERROR: %s\n' "$*" >&2; exit 1; }

cleanup_on_failure() {
  local rc=$?
  trap - ERR
  if [[ $rc -ne 0 ]]; then
    printf '\n[test] FAILED (rc=%s); cleaning up...\n' "$rc" >&2
    teardown_vm
  fi
  exit "$rc"
}
trap cleanup_on_failure ERR

# Ctrl-C reaches the whole foreground process group, so without this trap the
# script would die mid-socat before the cleanup ran.
on_interrupt() {
  trap - INT TERM
  printf '\n[test] interrupted\n' >&2
  if [[ "$KEEP_VM" != "1" ]]; then
    teardown_vm
  fi
  exit 130
}
trap on_interrupt INT TERM

teardown_vm() {
  [[ ${#ENGINE[@]} -gt 0 ]] || return 0
  if [[ -n "$VMID" ]]; then
    "${ENGINE[@]}" exec -i odorobo curl -s -X DELETE \
      "http://127.0.0.1:3000/vms/$VMID" >/dev/null 2>&1 || true
  fi
  # Defensive unmap in case the agent teardown did not release the RBD.
  rbd device unmap "$CEPH_POOL/$CEPH_IMAGE_NAME" --options noudev >/dev/null 2>&1 || true
}

# Run a command inside the odorobo container. Note: `exec` does NOT inherit
# the compose environment (CEPH_CONFIG et al.), and the ceph CLI reads
# CEPH_CONF/CEPH_KEYRING anyway, so rbd calls pass credentials explicitly.
in_container() { "${ENGINE[@]}" exec -i odorobo "$@"; }

# rbd with the generated credentials (mirrors the agent's own rbd invocations).
rbd() {
  in_container rbd \
    --conf="/generated/ceph.conf" \
    --id="$CEPH_CLIENT" \
    --keyfile="/generated/client.$CEPH_CLIENT.key" \
    --cluster=ceph "$@"
}

# --- 0. preflight ------------------------------------------------------------
[[ $(id -u) -eq 0 ]] || die "run as root (sudo bash $0)"

if command -v podman >/dev/null 2>&1 && podman compose version >/dev/null 2>&1; then
  ENGINE=(podman)
elif command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  ENGINE=(docker)
else
  die "podman (or docker) compose is required on the host"
fi

for tool in curl xz dd strings; do
  command -v "$tool" >/dev/null 2>&1 || die "host tool missing: $tool"
done
if ! command -v socat >/dev/null 2>&1 && ! command -v nc >/dev/null 2>&1; then
  die "need socat (or nc) on the host for the interactive console (dnf install -y socat)"
fi

log "checking the stack"
ceph_state=$("${ENGINE[@]}" inspect --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' odorobo-ceph 2>/dev/null || true)
[[ "$ceph_state" == running\ healthy* ]] || die "odorobo-ceph is not running+healthy (state: '$ceph_state'); run init.sh first"
odorobo_state=$("${ENGINE[@]}" inspect --format '{{.State.Status}}' odorobo 2>/dev/null || true)
[[ "$odorobo_state" == running* ]] || die "odorobo is not running (state: '$odorobo_state'); run init.sh first"
health=$(in_container curl -sf http://127.0.0.1:3000/health) || die "agent /health failed"
[[ "$health" == "OK" ]] || die "agent /health returned '$health'"
# The rbd mapping is a kernel object; make sure the module is loaded on the
# host (the container's modprobe would also load it, but fail early here).
[[ -d /sys/module/rbd ]] || modprobe rbd || die "rbd kernel module not available"
[[ -d /sys/module/loop ]] || modprobe loop || die "loop kernel module not available"
log "stack is up (engine: ${ENGINE[0]})"

# --- 1. assets ---------------------------------------------------------------
mkdir -p "$ASSET_DIR"

log "fetching firmware"
if [[ -f "$FIRMWARE_FILE" ]]; then
  fw_size=$(stat -c%s "$FIRMWARE_FILE")
  fw_magic=$(head -c4 "$FIRMWARE_FILE" | od -An -tx1 | tr -d ' \n')
  if [[ "$fw_size" == "$FIRMWARE_SIZE" && "$fw_magic" == "7f454c46" ]]; then
    echo "[test] firmware cached ($(sha256sum "$FIRMWARE_FILE" | cut -d' ' -f1))"
  else
    echo "[test] cached firmware failed verification; re-downloading"
    rm -f "$FIRMWARE_FILE"
  fi
fi
if [[ ! -f "$FIRMWARE_FILE" ]]; then
  curl -fL --retry 3 -o "$FIRMWARE_FILE" "$FIRMWARE_URL"
  fw_size=$(stat -c%s "$FIRMWARE_FILE")
  fw_magic=$(head -c4 "$FIRMWARE_FILE" | od -An -tx1 | tr -d ' \n')
  [[ "$fw_size" == "$FIRMWARE_SIZE" ]] || die "firmware size mismatch: $fw_size != $FIRMWARE_SIZE"
  [[ "$fw_magic" == "7f454c46" ]] || die "firmware is not an ELF (magic $fw_magic)"
  echo "[test] firmware downloaded ($(sha256sum "$FIRMWARE_FILE" | cut -d' ' -f1))"
fi

log "fetching Fedora image"
if [[ -f "$IMAGE_FILE" ]]; then
  echo "[test] verifying cached image sha256..."
  echo "$IMAGE_SHA256  $IMAGE_FILE" | sha256sum -c --quiet
else
  curl -fL --retry 3 -o "$IMAGE_FILE" "$IMAGE_URL"
  echo "$IMAGE_SHA256  $IMAGE_FILE" | sha256sum -c --quiet
fi

# Uncompressed size straight from the xz index (no decompression needed).
RAW_SIZE_BYTES=$(xz -l "$IMAGE_FILE" | awk 'NR == 2 {
  val = $5; unit = $6
  gsub(/,/, "", val)   # xz >= 5.8 prints thousands separators (5,120.0 MiB)
  if (unit == "KiB") print val * 1024
  else if (unit == "MiB") print val * 1024 * 1024
  else if (unit == "GiB") print val * 1024 * 1024 * 1024
  else print val
}')
[[ -n "$RAW_SIZE_BYTES" ]] || die "could not determine uncompressed size"
log "image decompresses to $((RAW_SIZE_BYTES / 1024 / 1024)) MiB"

# --- 2. write the image into the Ceph RBD ------------------------------------
if [[ "$SKIP_IMAGE_WRITE" == "1" ]]; then
  log "SKIP_IMAGE_WRITE=1; reusing the existing RBD contents"
else
  log "preparing the RBD image"
  current_size=$(rbd info --format=json "$CEPH_POOL/$CEPH_IMAGE_NAME" \
    | grep -o '"size":[[:space:]]*[0-9]*' | head -1 | grep -o '[0-9]*')
  [[ -n "$current_size" ]] || die "rbd info failed for $CEPH_POOL/$CEPH_IMAGE_NAME"
  if (( current_size < RAW_SIZE_BYTES )); then
    target_gib=$(( (RAW_SIZE_BYTES + 1073741824 - 1) / 1073741824 + 1 ))
    echo "[test] resizing $CEPH_POOL/$CEPH_IMAGE_NAME: $((current_size / 1024 / 1024)) MiB -> ${target_gib}G"
    rbd resize "$CEPH_POOL/$CEPH_IMAGE_NAME" --size "${target_gib}G"
  else
    echo "[test] RBD is $((current_size / 1024 / 1024)) MiB; no resize needed"
  fi

  # Clear any stale mapping (e.g. left by a previous run or a reset).
  rbd device unmap "$CEPH_POOL/$CEPH_IMAGE_NAME" --options noudev >/dev/null 2>&1 || true

  log "mapping the RBD and writing the image from the host"
  # Map with noudev: with udev enabled, the rbd CLI waits for a udev event
  # in its own network namespace, which never arrives for a host-created
  # rbd device inside a container. The host's udevd still creates the
  # stable path, so wait for it and fall back to the kernel name.
  DEV_PATH=$(rbd device map "$CEPH_POOL/$CEPH_IMAGE_NAME" --options noudev)
  for _ in $(seq 1 30); do
    if [[ -e "/dev/rbd/$CEPH_POOL/$CEPH_IMAGE_NAME" ]]; then
      DEV_PATH="/dev/rbd/$CEPH_POOL/$CEPH_IMAGE_NAME"
      break
    fi
    sleep 0.5
  done
  echo "[test] writing to $DEV_PATH"
  # The mapping is a kernel object and /dev is shared with the host, so a
  # host-side dd writes straight into the pool. `xz -dc` keeps the .xz and
  # sends the decompressed stream to stdout (newer xz writes to a file
  # otherwise, which would starve the pipe).
  xz -dc "$IMAGE_FILE" | dd of="$DEV_PATH" bs=1M status=progress
  sync
  # Unmap so the agent performs the mapping itself when the VM is created
  # (the application path under test; see README.md).
  rbd device unmap "$CEPH_POOL/$CEPH_IMAGE_NAME" --options noudev
  log "image written to $CEPH_POOL/$CEPH_IMAGE_NAME"
fi

# --- 3. create the VM (firmware boot) ----------------------------------------
gen_ulid() {
  local hex
  hex=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
  local alphabet="0123456789ABCDEFGHJKMNPQRSTVWXYZ"
  local out="" i b p byte bit byteval bitval n
  for ((i = 0; i < 26; i++)); do
    n=0
    for ((b = 4; b >= 0; b--)); do
      # 130-bit window (2 pad bits + 128-bit value); 128-bit position ==
      # 130-bit position for positions 0..127.
      p=$(( 125 - 5 * i + b ))
      if (( p > 127 )); then
        bitval=0
      else
        byte=$(( 15 - p / 8 ))
        bit=$(( p % 8 ))
        byteval=$(( 16#${hex:byte*2:2} ))
        bitval=$(( (byteval >> bit) & 1 ))
      fi
      n=$(( (n << 1) | bitval ))
    done
    out+="${alphabet:n:1}"
  done
  printf '%s\n' "$out"
}

log "creating the VM"
VMID=$(gen_ulid)
MANIFEST=$(cat <<EOF
{
  "vm": {
    "api_version": 1,
    "id": "$VMID",
    "desired": {
      "metadata": { "name": "$VM_NAME", "labels": {}, "annotations": {} },
      "compute": { "vcpus": $VM_VCPUS, "memory_bytes": $((VM_MEMORY_GB * 1073741824)) },
      "storage": [ { "id": "root", "uri": "rbd://$CEPH_POOL/$CEPH_IMAGE_NAME" } ],
      "networks": [],
      "placement": {},
      "boot": { "start": true, "firmware": "$FIRMWARE_PATH_IN_CONTAINER" }
    }
  }
}
EOF
)
RESPONSE=$(printf '%s' "$MANIFEST" | in_container curl -s -X POST \
  -H 'Content-Type: application/json' --data-binary @- http://127.0.0.1:3000/vms)
echo "$RESPONSE" | head -c 400; echo
# The agent returns CreateVMReply {"config":..., "actor_id":...} on success
# and {"message":...} on error; requiring "actor_id" also catches opaque
# 500 error bodies.
if ! grep -q '"actor_id"' <<<"$RESPONSE"; then
  die "agent create failed: $RESPONSE"
fi
if grep -q '"actor_id":null' <<<"$RESPONSE"; then
  die "agent accepted the manifest but created no VM actor: $RESPONSE"
fi
log "VM created: $VMID"
echo "    console (in the odorobo container): /run/odorobo/vms/$VMID/console.sock"

# --- 4. watch the boot --------------------------------------------------------
log "waiting for the firmware boot (up to ${BOOT_TIMEOUT}s)"
saw_firmware=0 saw_efi=0 saw_kernel=0 saw_login=0
deadline=$(( $(date +%s) + BOOT_TIMEOUT ))
while (( $(date +%s) < deadline )); do
  history=$(in_container curl -s "http://127.0.0.1:3000/vms/$VMID/console/history" | strings || true)
  if (( ! saw_firmware )) && grep -q "Booting with" <<<"$history"; then
    saw_firmware=1; echo "[test] firmware is running"
  fi
  if (( ! saw_efi )) && grep -qE "Found EFI partition|Using EFI boot|Found bootloader|Jumping to kernel" <<<"$history"; then
    saw_efi=1; echo "[test] firmware found the bootloader on the RBD disk"
  fi
  if (( ! saw_kernel )) && grep -q "Linux version" <<<"$history"; then
    saw_kernel=1; echo "[test] kernel is booting"
  fi
  if grep -q "login:" <<<"$history"; then
    saw_login=1
    break
  fi
  sleep 5
done

if (( saw_login )); then
  log "login prompt reached -- the VM booted through the firmware"
else
  log "no login prompt within ${BOOT_TIMEOUT}s (firmware=$saw_firmware efi=$saw_efi kernel=$saw_kernel); attaching anyway so you can inspect"
  echo "[test] last console output:"
  in_container curl -s "http://127.0.0.1:3000/vms/$VMID/console/history" | strings | tail -30
fi

# --- 5. interactive console ----------------------------------------------------
log "attaching the interactive serial console"
echo
echo "  Log in with:  user=fedora  password=fedora"
echo "  (if the image locks password login, the login prompt itself still"
echo "   proves the OS booted; a keypair via cloud-init would be needed)"
echo "  Detach with:  Ctrl-C"
echo
# The agent's runtime dir is container-local (no host bind mount), so relay
# the serial socket through the odorobo container. CH's serial socket serves
# one client at a time: connecting here takes the console over from the
# agent's spool.
CONSOLE_SOCKET="/run/odorobo/vms/$VMID/console.sock"
if "${ENGINE[@]}" exec -i odorobo sh -c 'command -v socat' >/dev/null 2>&1; then
  "${ENGINE[@]}" exec -i odorobo socat -,raw,echo=0 "UNIX-CONNECT:$CONSOLE_SOCKET" || true
elif "${ENGINE[@]}" exec -i odorobo sh -c 'command -v python3' >/dev/null 2>&1; then
  "${ENGINE[@]}" exec -i odorobo python3 -c '
import os, select, socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
while True:
    r, _, _ = select.select([0, s], [], [])
    for src in r:
        data = os.read(0, 4096) if src == 0 else s.recv(4096)
        if not data:
            os._exit(0)
        if src == 0:
            os.write(1, data)
        else:
            s.sendall(data)
' "$CONSOLE_SOCKET" || true
else
  echo "[test] neither socat nor python3 is available in the container;"
  echo "      re-attach manually once one is installed, e.g. with socat:"
  echo "    ${ENGINE[0]} exec -it odorobo sh -c \"socat -,raw,echo=0 UNIX-CONNECT:$CONSOLE_SOCKET\""
fi

# --- 6. cleanup ----------------------------------------------------------------
log "detached"
if [[ "$KEEP_VM" == "1" ]]; then
  log "KEEP_VM=1 -- leaving the VM running"
  echo "  re-attach with:"
  echo "    socat -,raw,echo=0 UNIX-CONNECT:/run/odorobo/vms/$VMID/console.sock"
  echo "  delete later with:"
  echo "    ${ENGINE[0]} exec -i odorobo curl -s -X DELETE http://127.0.0.1:3000/vms/$VMID"
  exit 0
fi

if [[ -t 0 ]]; then
  read -r -p "Delete the VM and unmap the RBD? [Y/n] " answer
  answer=${answer:-Y}
else
  answer=Y
fi
if [[ "${answer^^}" == "Y" ]]; then
  log "deleting the VM"
  "${ENGINE[@]}" exec -i odorobo curl -s -X DELETE "http://127.0.0.1:3000/vms/$VMID" >/dev/null
  rbd device unmap "$CEPH_POOL/$CEPH_IMAGE_NAME" --options noudev >/dev/null 2>&1 || true
  log "done. The RBD image still contains the Fedora image, so the next run can use SKIP_IMAGE_WRITE=1"
else
  log "leaving the VM running (re-attach command above still applies)"
fi
