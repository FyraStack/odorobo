#!/usr/bin/env bash
# Start the ceph-backed test VM.
#
# Run this from INSIDE the odorobo container, where 127.0.0.1:3000 is the
# agent (the container shares Ceph's network namespace):
#   sudo podman compose -f .local/dev/compose.yml exec odorobo \
#     bash .local/dev/vm-test.sh
#
# Prerequisites (already done manually this session):
#   - /tmp/vmlinuz-virt present in the container (Alpine virt kernel)
#   - rbd image mapped: /dev/rbd/odorobo-blockpool/dev-disk -> /dev/nbd0
#   - rootfs dd'd into that device
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo ">> health check"
curl -s http://127.0.0.1:3000/health; echo

echo ">> POST /vms"
curl -s -X POST http://127.0.0.1:3000/vms \
  -H 'Content-Type: application/json' \
  -d "@$DIR/vm-test.json"
echo

echo ">> waiting for boot, then dumping console history"
sleep 15
curl -s http://127.0.0.1:3000/vms/01J0TESTVM0000000000000001/console/history \
  | strings | tail -40
