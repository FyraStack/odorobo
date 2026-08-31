#!/usr/bin/env bash
set -euo pipefail

: "${CEPH_IMAGE:=quay.io/ceph/ceph:v20.2.3}"
: "${CEPH_MON_IP:=127.0.0.1}"
: "${CEPH_POOL:=odorobo-blockpool}"
: "${CEPH_CLIENT:=odorobo}"
: "${CEPH_IMAGE_NAME:=dev-disk}"
: "${CEPH_IMAGE_SIZE:=1G}"
: "${CEPH_OSD_SIZE:=10G}"

mkdir -p /etc/ceph /var/lib/ceph /var/log/ceph /run/ceph /var/lib/odorobo-ceph

if [[ ! -f /etc/ceph/ceph.conf ]]; then
  cephadm --image "$CEPH_IMAGE" bootstrap \
    --mon-ip "$CEPH_MON_IP" \
    --single-host-defaults \
    --output-dir /etc/ceph \
    --skip-monitoring-stack \
    --allow-fqdn-hostname \
    --skip-pull
fi

# cephadm has created and started the daemons. Keep this container alive while
# allowing compose stop/restart to control the complete development cluster.
while ! ceph -s >/dev/null 2>&1; do
  sleep 2
done

if ! ceph osd ls 2>/dev/null | grep -q '[0-9]'; then
  truncate -s "$CEPH_OSD_SIZE" /var/lib/odorobo-ceph/osd.raw
  OSD_DEVICE=$(losetup --find --show /var/lib/odorobo-ceph/osd.raw)
  echo "$OSD_DEVICE" > /var/lib/odorobo-ceph/osd.device
  cephadm shell -- ceph-volume raw prepare --data "$OSD_DEVICE"
  cephadm shell -- ceph-volume raw activate --device "$OSD_DEVICE" --no-systemd &
fi

for _ in {1..60}; do
  if ceph osd stat 2>/dev/null | grep -q '[1-9][0-9]* osds:'; then
    break
  fi
  sleep 2
done

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

ceph config generate-minimal-conf > /var/lib/odorobo-ceph/ceph.conf
ceph auth get-key "client.$CEPH_CLIENT" > /var/lib/odorobo-ceph/client.$CEPH_CLIENT.key
chmod 600 /var/lib/odorobo-ceph/client.$CEPH_CLIENT.key
cp /var/lib/odorobo-ceph/ceph.conf /generated/ceph.conf
cp /var/lib/odorobo-ceph/client.$CEPH_CLIENT.key /generated/client.$CEPH_CLIENT.key
chmod 600 /generated/client.$CEPH_CLIENT.key

tail -f /dev/null
