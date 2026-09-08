#!/usr/bin/env bash
set -euo pipefail

: "${CEPH_MON_IP:=127.0.0.1}"
: "${CEPH_POOL:=odorobo-blockpool}"
: "${CEPH_CLIENT:=odorobo}"
: "${CEPH_IMAGE_NAME:=dev-disk}"
: "${CEPH_IMAGE_SIZE:=1G}"
: "${CEPH_OSD_SIZE:=10G}"

CLUSTER=ceph
MON_ID=ceph
MGR_ID=ceph
CEPH_CONF=/etc/ceph/ceph.conf
CEPH_DATA_DIR=/var/lib/ceph
OSD_STATE_DIR=/var/lib/odorobo-ceph
OSD_DATA_DIR="$OSD_STATE_DIR/osd"
OSD_IMAGE="$OSD_STATE_DIR/osd.raw"
OSD_ID_FILE="$OSD_STATE_DIR/osd.id"
OSD_UUID_FILE="$OSD_STATE_DIR/osd.uuid"
LOOP_DEVICE=""
MON_PID=""
MGR_PID=""
OSD_PID=""

cleanup() {
  local pid
  trap - EXIT INT TERM
  for pid in "$OSD_PID" "$MGR_PID" "$MON_PID"; do
    [[ -n "$pid" ]] || continue
    kill -TERM "$pid" 2>/dev/null || true
  done
  for pid in "$OSD_PID" "$MGR_PID" "$MON_PID"; do
    [[ -n "$pid" ]] || continue
    wait "$pid" 2>/dev/null || true
  done
  [[ -n "$LOOP_DEVICE" ]] && losetup --detach "$LOOP_DEVICE" 2>/dev/null || true
  }
trap cleanup EXIT INT TERM

mkdir -p /etc/ceph "$CEPH_DATA_DIR" /var/log/ceph /run/ceph "$OSD_STATE_DIR" /generated
chown ceph:ceph /run/ceph

# These paths are bind-mounted from the host and may retain ownership from a
# previous interrupted or root-run initialization.
for directory in \
  "$CEPH_DATA_DIR/mon" \
  "$CEPH_DATA_DIR/mgr" \
  "$CEPH_DATA_DIR/bootstrap-osd" \
  "$CEPH_DATA_DIR/osd" \
  "$OSD_STATE_DIR"; do
  [[ -d "$directory" ]] && chown -R ceph:ceph "$directory"
done

echo "[odorobo-ceph] initializing direct Ceph daemons"
if [[ ! -f "$CEPH_CONF" ]]; then
  echo "[odorobo-ceph] creating monitor configuration and keyrings"
  FSID=$(uuidgen)
  cat >"$CEPH_CONF" <<EOF
[global]
fsid = $FSID
mon initial members = $MON_ID
mon host = $CEPH_MON_IP

# This development cluster has one host and one OSD only.
mon allow pool size one = true
osd pool default size = 1
osd pool default min size = 1
osd crush chooseleaf type = 0
osd objectstore = bluestore

[mgr]
# This stack runs daemons directly and does not use cephadm orchestration.
# The direct-daemon container does not provide the host udev/system services used
# by optional MGR Python modules. RBD and Ceph CLI operations do not require them.
mgr disabled modules = alerts,balancer,cephadm,crash,dashboard,devicehealth,diskprediction_local,dynatrace,influx,insights,iostat,k8sevents,loki,nfs,orchestrator,pg_autoscaler,prometheus,restful,selftest,snap_schedule,stats,telemetry,telegraf,volumes,zabbix
EOF

  install -d -o ceph -g ceph "$CEPH_DATA_DIR/mon/$CLUSTER-$MON_ID"
  install -d -o ceph -g ceph "$CEPH_DATA_DIR/mgr/$CLUSTER-$MGR_ID"
  install -d -o ceph -g ceph "$CEPH_DATA_DIR/bootstrap-osd"

  ceph-authtool --create-keyring /tmp/ceph.mon.keyring --gen-key -n mon.
  ceph-authtool --create-keyring \
    /etc/ceph/ceph.client.admin.keyring \
    --gen-key -n client.admin \
    --cap mon 'allow *' \
    --cap osd 'allow *' \
    --cap mds 'allow *' \
    --cap mgr 'allow *'
  ceph-authtool --create-keyring \
    "$CEPH_DATA_DIR/bootstrap-osd/$CLUSTER.keyring" \
    --gen-key -n client.bootstrap-osd \
    --cap mon 'profile bootstrap-osd' \
    --cap mgr 'allow r'
  ceph-authtool /tmp/ceph.mon.keyring --import-keyring /etc/ceph/ceph.client.admin.keyring
  ceph-authtool /tmp/ceph.mon.keyring \
    --import-keyring "$CEPH_DATA_DIR/bootstrap-osd/$CLUSTER.keyring"

  monmaptool --create --add "$MON_ID" "$CEPH_MON_IP" --fsid "$FSID" /tmp/monmap
  chown ceph:ceph /tmp/ceph.mon.keyring /etc/ceph/ceph.client.admin.keyring \
    "$CEPH_DATA_DIR/bootstrap-osd/$CLUSTER.keyring"
  ceph-mon --mkfs -i "$MON_ID" --monmap /tmp/monmap --keyring /tmp/ceph.mon.keyring
  chown -R ceph:ceph "$CEPH_DATA_DIR/mon/$CLUSTER-$MON_ID"
  rm -f /tmp/ceph.mon.keyring /tmp/monmap
fi

echo "[odorobo-ceph] starting monitor"
ceph-mon -f -i "$MON_ID" --setuser ceph --setgroup ceph &
MON_PID=$!

for _ in {1..60}; do
  if timeout 5 ceph -s >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$MON_PID" 2>/dev/null; then
    wait "$MON_PID"
  fi
  sleep 1
done
if ! timeout 5 ceph -s; then
  echo "Monitor did not become ready within 60 seconds." >&2
  exit 1
fi

echo "[odorobo-ceph] monitor ready; preparing OSD"

if [[ -f "$OSD_ID_FILE" ]]; then
  OSD_ID=$(<"$OSD_ID_FILE")
  OSD_UUID=$(<"$OSD_UUID_FILE")
else
  OSD_UUID=$(uuidgen)
  OSD_SECRET=$(ceph-authtool --gen-print-key)
  OSD_ID=$(printf '{"cephx_secret": "%s"}\n' "$OSD_SECRET" \
    | ceph osd new "$OSD_UUID" -i - -n client.bootstrap-osd \
      -k "$CEPH_DATA_DIR/bootstrap-osd/$CLUSTER.keyring")
  [[ -n "$OSD_ID" ]] || { echo "Ceph did not register the OSD." >&2; exit 1; }
  mkdir -p "$OSD_DATA_DIR"
  ceph-authtool --create-keyring "$OSD_DATA_DIR/keyring" \
    --name "osd.$OSD_ID" --add-key "$OSD_SECRET"
  truncate -s "$CEPH_OSD_SIZE" "$OSD_IMAGE"
  LOOP_DEVICE=$(losetup --find --show "$OSD_IMAGE") || {
    echo "Unable to attach the OSD image to a loop device." >&2
    exit 1
  }
  # The host-created loop node is normally root:disk; the OSD daemon drops to
  # the ceph user and therefore needs direct access to this block device.
  chown ceph:ceph "$LOOP_DEVICE"
  chmod 0660 "$LOOP_DEVICE"
  ln -sfn "$LOOP_DEVICE" "$OSD_DATA_DIR/block"
  ceph-osd -i "$OSD_ID" --mkfs --osd-uuid "$OSD_UUID" \
    --osd-data "$OSD_DATA_DIR"
  printf '%s\n' "$OSD_ID" >"$OSD_ID_FILE"
  printf '%s\n' "$OSD_UUID" >"$OSD_UUID_FILE"
fi

if [[ -z "$LOOP_DEVICE" ]]; then
  LOOP_DEVICE=$(losetup --associated --noheadings --output NAME "$OSD_IMAGE" | awk 'NR == 1 { print; exit }')
  [[ -n "$LOOP_DEVICE" ]] || LOOP_DEVICE=$(losetup --find --show "$OSD_IMAGE") || {
    echo "Unable to attach the OSD image to a loop device." >&2
    exit 1
  }
  chown ceph:ceph "$LOOP_DEVICE"
  chmod 0660 "$LOOP_DEVICE"
  ln -sfn "$LOOP_DEVICE" "$OSD_DATA_DIR/block"
fi

chown -R ceph:ceph "$OSD_DATA_DIR"
ceph-osd -f -i "$OSD_ID" --osd-data "$OSD_DATA_DIR" \
  --setuser ceph --setgroup ceph &
OSD_PID=$!

echo "[odorobo-ceph] preparing and starting OSD"
for _ in {1..60}; do
  if timeout 5 ceph osd stat 2>/dev/null | grep -q '[1-9][0-9]* up'; then
    break
  fi
  if ! kill -0 "$OSD_PID" 2>/dev/null; then
    wait "$OSD_PID"
  fi
  sleep 1
done
if ! timeout 5 ceph osd stat 2>/dev/null | grep -q '[1-9][0-9]* up'; then
  echo "OSD did not become up within 60 seconds." >&2
  exit 1
fi

echo "[odorobo-ceph] OSD ready; provisioning RBD"
ceph config set mon mon_allow_pool_size_one true
ceph config set osd osd_pool_default_size 1
ceph config set osd osd_pool_default_min_size 1
if ! ceph osd pool ls --format=json | grep -qF "\"$CEPH_POOL\""; then
  ceph osd pool create "$CEPH_POOL" 8
fi
ceph osd pool set "$CEPH_POOL" size 1 --yes-i-really-mean-it
ceph osd pool set "$CEPH_POOL" min_size 1
rbd pool init "$CEPH_POOL"

if ! ceph auth get "client.$CEPH_CLIENT" >/dev/null 2>&1; then
  ceph auth get-or-create "client.$CEPH_CLIENT" \
    mon 'allow r' osd "allow rwx pool=$CEPH_POOL" >/dev/null
fi
if ! rbd info "$CEPH_POOL/$CEPH_IMAGE_NAME" >/dev/null 2>&1; then
  rbd create "$CEPH_POOL/$CEPH_IMAGE_NAME" --size "$CEPH_IMAGE_SIZE"
fi

ceph config generate-minimal-conf >"$OSD_STATE_DIR/ceph.conf"
ceph auth get-key "client.$CEPH_CLIENT" >"$OSD_STATE_DIR/client.$CEPH_CLIENT.key"
chmod 600 "$OSD_STATE_DIR/client.$CEPH_CLIENT.key"
cp "$OSD_STATE_DIR/ceph.conf" /generated/ceph.conf
cp "$OSD_STATE_DIR/client.$CEPH_CLIENT.key" /generated/client."$CEPH_CLIENT".key
chmod 600 /generated/client."$CEPH_CLIENT".key

wait "$MON_PID" "$MGR_PID" "$OSD_PID"
