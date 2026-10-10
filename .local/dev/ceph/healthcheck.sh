#!/usr/bin/env bash
set -euo pipefail

: "${CEPH_POOL:=odorobo-blockpool}"
: "${CEPH_CLIENT:=odorobo}"
: "${CEPH_IMAGE_NAME:=dev-disk}"

READY_MARKER=/generated/.odorobo-ceph-ready
CEPH_CONFIG=/generated/ceph.conf
CEPH_KEYFILE="/generated/client.${CEPH_CLIENT}.key"

[[ -f "$READY_MARKER" && -s "$CEPH_CONFIG" && -s "$CEPH_KEYFILE" ]] || exit 1

# A responsive monitor alone is insufficient: verify the generated client can
# authenticate and access the provisioned image Odorobo will use.
ceph --conf="$CEPH_CONFIG" --id="$CEPH_CLIENT" --keyfile="$CEPH_KEYFILE" -s >/dev/null
rbd --conf="$CEPH_CONFIG" --id="$CEPH_CLIENT" --keyfile="$CEPH_KEYFILE" \
  info "$CEPH_POOL/$CEPH_IMAGE_NAME" >/dev/null
