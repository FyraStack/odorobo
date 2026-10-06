#!/usr/bin/env bash
# Build a tiny Linux kernel for odorobo FaaS microVMs (issue #112).
#
# Boot path: Cloud Hypervisor direct kernel boot (PVH) of the vmlinux ELF,
# no firmware, no GRUB, no initramfs — the root filesystem is a virtiofs
# mount provided by virtiofsd (tag "rootfs"), so the virtio/virtiofs and
# overlayfs drivers MUST be built-in (=y), not modules.
#
# Config is x86_64_defconfig (the issue's buildconfig) plus:
#   * CONFIG_PVH — paravirt boot entry for CH direct boot
#   * virtio: PCI, NET, BLK, CONSOLE, BALLOON (+ VSOCKETS for odorobo vsock)
#   * OVERLAY_FS, FUSE_FS, VIRTIO_FS — OCI rootfs via virtiofs
#   * "small + fast boot" trims: no modules, no debug info/BTF, and the big
#     unused subsystems (sound/DRM/USB/HID/wifi/vendor ethernet/staging) off.
#     A trimmed build is ~1/4 the size of a stock defconfig vmlinux and
#     boots in well under a second under KVM.
#
# Usage: scripts/build-microvm-kernel.sh [OUTPUT_DIR]
#   KERNEL_VERSION=6.12.x overrides the version (default: latest longterm).
#   OUTPUT_DIR defaults to /var/lib/odorobo/microvm (installs vmlinux).

set -euo pipefail

OUT_DIR="${1:-/var/lib/odorobo/microvm}"
SRC_PARENT="${MICROVM_KERNEL_SRC:-/var/lib/odorobo/microvm/src}"

# --- resolve version (default: kernel.org's current longterm) ---
if [[ -z "${KERNEL_VERSION:-}" ]]; then
    KERNEL_VERSION="$(curl -fsSL https://www.kernel.org/releases.json |
        python3 -c 'import json,sys; rs=json.load(sys.stdin)["releases"]; print(next(r["version"] for r in rs if r["moniker"]=="longterm"))')"
fi

JOBS="$(nproc)"
SRC_DIR="$SRC_PARENT/linux-$KERNEL_VERSION"
TARBALL="$SRC_PARENT/linux-$KERNEL_VERSION.tar.xz"

mkdir -p "$SRC_PARENT" "$OUT_DIR"

# --- fetch source ---
if [[ ! -f "$SRC_DIR/Makefile" ]]; then
    echo "==> fetching linux $KERNEL_VERSION"
    rm -f "$TARBALL"
    curl -fL -o "$TARBALL" "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KERNEL_VERSION.tar.xz"
    rm -rf "$SRC_DIR"
    tar -xJf "$TARBALL" -C "$SRC_PARENT"
fi

cd "$SRC_DIR"

echo "==> configuring (x86_64_defconfig + microvm trims)"
make O="$SRC_DIR/.build" x86_64_defconfig
CFG() { ./scripts/config --file "$SRC_DIR/.build/.config" "$@"; }

# paravirt boot entry for CH direct boot (issue #112)
CFG --enable PVH
# virtio (built-in: needed to mount root over virtiofs with no initramfs)
CFG --enable VIRTIO
CFG --enable VIRTIO_PCI
CFG --enable VIRTIO_NET
CFG --enable VIRTIO_BLK
CFG --enable VIRTIO_CONSOLE
CFG --enable VIRTIO_BALLOON
# overlayfs (container root semantics for the unpacked OCI image)
CFG --enable OVERLAY_FS
# composefs (future rootfs backend: fs-verity verified, deduplicated roots)
CFG --enable FS_COMPOSEFS
# FUSE and virtiofs (the root filesystem)
CFG --enable FUSE_FS
CFG --enable VIRTIO_FS
# vsock (odorobo vsock attachment)
CFG --enable VSOCKETS
CFG --enable VIRTIO_VSOCKETS
# serial console
CFG --enable SERIAL_8250
CFG --enable SERIAL_8250_CONSOLE

# --- "very small + boots fast" trims ---
CFG -d MODULES                 # everything built-in, no module loader
CFG -d DEBUG_INFO              # no DWARF (saves ~60% of vmlinux size)
CFG --enable DEBUG_INFO_NONE
CFG -d DEBUG_KERNEL
CFG -d GDB_SCRIPTS
CFG -d KALLSYMS_ALL
CFG --set-str LOCALVERSION "-odorobo-microvm"
CFG --set-str SYSTEM_TRUSTED_KEYS ""
# subsystems a headless FaaS microVM never touches
CFG -d SOUND
CFG -d DRM
CFG -d USB
CFG -d HID_SUPPORT
CFG -d WLAN
CFG -d ETHERNET                # vendor NICs off; VIRTIO_NET is not under ETHERNET
CFG -d STAGING
CFG -d FIRMWARE_LOADER
CFG -d MEDIA_SUPPORT
CFG -d GAMEPORT
CFG -d JOYSTICK
CFG -d TOUCHSCREEN

make O="$SRC_DIR/.build" olddefconfig
echo "==> building vmlinux (-j$JOBS)"
START=$SECONDS
make O="$SRC_DIR/.build" -j"$JOBS" vmlinux

KERNEL="$SRC_DIR/.build/vmlinux"
if [[ ! -f "$KERNEL" ]]; then
    echo "error: kernel build finished but $KERNEL is missing" >&2
    exit 1
fi

# Install a stripped copy: CH loads the ELF via PVH and symbols only pad
# the file (40 MB unstripped -> ~29 MB stripped).
install -d "$OUT_DIR"
if strip -s -o "$OUT_DIR/vmlinux" "$KERNEL" 2>/dev/null || cp "$KERNEL" "$OUT_DIR/vmlinux"; then
    chmod 755 "$OUT_DIR/vmlinux"
else
    echo "error: failed to install $KERNEL to $OUT_DIR/vmlinux" >&2
    exit 1
fi

echo "==> done in $((SECONDS - START))s"
echo "   $(du -h "$OUT_DIR/vmlinux" | cut -f1) -> $OUT_DIR/vmlinux"
echo "   boot it with: cloud-hypervisor --kernel $OUT_DIR/vmlinux --cmdline \"console=ttyS0 root=rootfs rootfstype=virtiofs rw\" ..."
