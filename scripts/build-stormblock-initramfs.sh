#!/bin/bash
# build-stormblock-initramfs.sh — Build a minimal LinuxBoot-style initramfs
#
# Creates a self-contained initramfs containing:
#   /init               — Boot init script (busybox sh)
#   /usr/sbin/stormblock — Static binary
#   /bin/busybox         — Shell + basic tools
#   /lib/modules/        — ublk_drv kernel module
#   /dev, /proc, /sys, /sysroot — mount points
#
# Usage:
#   ./scripts/build-stormblock-initramfs.sh [stormblock-binary] [kernel-version]
#
# Defaults:
#   stormblock-binary = target/x86_64-unknown-linux-musl/release/stormblock
#   kernel-version    = $(uname -r)
#
# Output: /tmp/stormblock-initramfs.img (zstd-compressed cpio)
#
# Requirements: busybox (static), cpio, zstd

set -euo pipefail

STORMBLOCK_BIN="${1:-target/x86_64-unknown-linux-musl/release/stormblock}"
KVER="${2:-$(uname -r)}"
OUTPUT="${3:-/tmp/stormblock-initramfs.img}"

if [ ! -f "$STORMBLOCK_BIN" ]; then
    echo "ERROR: stormblock binary not found: $STORMBLOCK_BIN"
    echo "Build it first: cargo build --release --target x86_64-unknown-linux-musl"
    exit 1
fi

# Find busybox (static)
BUSYBOX=""
for candidate in /usr/bin/busybox /bin/busybox /usr/sbin/busybox; do
    if [ -x "$candidate" ]; then
        BUSYBOX="$candidate"
        break
    fi
done
if [ -z "$BUSYBOX" ]; then
    echo "ERROR: busybox not found"
    exit 1
fi

echo "Building stormblock-initramfs..."
echo "  stormblock: $STORMBLOCK_BIN ($(du -h "$STORMBLOCK_BIN" | cut -f1))"
echo "  busybox:    $BUSYBOX"
echo "  kernel:     $KVER"
echo "  output:     $OUTPUT"

# Create temporary initramfs root
INITRD_DIR=$(mktemp -d)
trap 'rm -rf "$INITRD_DIR"' EXIT

mkdir -p "$INITRD_DIR"/{bin,sbin,usr/sbin,lib/modules,dev,proc,sys,sysroot,etc,run,tmp,var}

# Busybox (static) + symlinks
cp "$BUSYBOX" "$INITRD_DIR/bin/busybox"
chmod 755 "$INITRD_DIR/bin/busybox"
for cmd in sh mount umount insmod modprobe ip cat grep cut sleep ln mkdir \
           swapon switch_root mdev udhcpc dmesg echo printf true false test \
           ls rm cp mv chmod chown mknod; do
    ln -s busybox "$INITRD_DIR/bin/$cmd"
done

# StormBlock binary
cp "$STORMBLOCK_BIN" "$INITRD_DIR/usr/sbin/stormblock"
chmod 755 "$INITRD_DIR/usr/sbin/stormblock"

# ublk kernel module (try compressed and uncompressed)
UBLK_FOUND=false
for path in \
    "/lib/modules/$KVER/kernel/drivers/block/ublk_drv.ko" \
    "/lib/modules/$KVER/kernel/drivers/block/ublk_drv.ko.xz" \
    "/lib/modules/$KVER/kernel/drivers/block/ublk_drv.ko.zst" \
    "/lib/modules/$KVER/kernel/drivers/block/ublk_drv.ko.gz"; do
    if [ -f "$path" ]; then
        cp "$path" "$INITRD_DIR/lib/modules/"
        UBLK_FOUND=true
        echo "  ublk_drv:   $path"
        break
    fi
done
if [ "$UBLK_FOUND" = false ]; then
    echo "  WARNING: ublk_drv.ko not found for kernel $KVER"
    echo "           The initramfs will try modprobe at boot time."
fi

# Minimal /etc
cat > "$INITRD_DIR/etc/mdev.conf" << 'MDEV'
ublk[bc].* 0:0 0660
MDEV

# /init script — the LinuxBoot entry point
cat > "$INITRD_DIR/init" << 'INITSCRIPT'
#!/bin/sh
# StormBlock LinuxBoot init
# Connects to iSCSI, creates ublk devices, mounts root, switch_root to systemd.

export PATH=/bin:/sbin:/usr/sbin

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts
mount -t devpts devpts /dev/pts

# Parse kernel cmdline parameters
PORTAL=""
IQN=""
LAYOUT=""
PORT="3260"
IP_CONF=""

for param in $(cat /proc/cmdline); do
    case "$param" in
        rd.stormblock.portal=*) PORTAL="${param#*=}" ;;
        rd.stormblock.iqn=*)    IQN="${param#*=}" ;;
        rd.stormblock.layout=*) LAYOUT="${param#*=}" ;;
        rd.stormblock.port=*)   PORT="${param#*=}" ;;
        ip=*)                   IP_CONF="${param#*=}" ;;
    esac
done

# Validate required parameters
if [ -z "$PORTAL" ] || [ -z "$IQN" ] || [ -z "$LAYOUT" ]; then
    echo "FATAL: Missing required kernel parameters:"
    echo "  rd.stormblock.portal=$PORTAL"
    echo "  rd.stormblock.iqn=$IQN"
    echo "  rd.stormblock.layout=$LAYOUT"
    echo "Dropping to shell..."
    exec /bin/sh
fi

echo "StormBlock LinuxBoot init"
echo "  Portal: $PORTAL:$PORT"
echo "  IQN:    $IQN"
echo "  Layout: $LAYOUT"

# Load ublk driver
if [ -f /lib/modules/ublk_drv.ko ]; then
    insmod /lib/modules/ublk_drv.ko 2>/dev/null
elif [ -f /lib/modules/ublk_drv.ko.xz ]; then
    xzcat /lib/modules/ublk_drv.ko.xz > /tmp/ublk_drv.ko && insmod /tmp/ublk_drv.ko 2>/dev/null
elif [ -f /lib/modules/ublk_drv.ko.zst ]; then
    zstdcat /lib/modules/ublk_drv.ko.zst > /tmp/ublk_drv.ko && insmod /tmp/ublk_drv.ko 2>/dev/null
else
    modprobe ublk_drv 2>/dev/null
fi

if [ ! -c /dev/ublk-control ]; then
    echo "WARNING: /dev/ublk-control not found — ublk_drv may not be loaded"
fi

# Network setup
echo "Configuring network..."
ip link set lo up

# Find first non-loopback interface
IFACE=""
for dev in /sys/class/net/*; do
    name=$(basename "$dev")
    [ "$name" = "lo" ] && continue
    IFACE="$name"
    break
done

if [ -z "$IFACE" ]; then
    echo "FATAL: No network interface found"
    exec /bin/sh
fi

ip link set "$IFACE" up

if [ -n "$IP_CONF" ]; then
    # Static IP from kernel cmdline (ip=addr::gw:mask::iface:none)
    ADDR=$(echo "$IP_CONF" | cut -d: -f1)
    GW=$(echo "$IP_CONF" | cut -d: -f3)
    MASK=$(echo "$IP_CONF" | cut -d: -f4)
    ip addr add "$ADDR/$MASK" dev "$IFACE"
    [ -n "$GW" ] && ip route add default via "$GW"
else
    # DHCP
    udhcpc -i "$IFACE" -s /bin/true -q -n -t 10 2>/dev/null
    if [ $? -ne 0 ]; then
        echo "WARNING: DHCP failed, trying link-local..."
        ip addr add 169.254.1.1/16 dev "$IFACE"
    fi
fi

echo "Network: $(ip addr show "$IFACE" | grep 'inet ' | awk '{print $2}')"

# Start stormblock boot-iscsi with ublk export
echo "Starting StormBlock..."
/usr/sbin/stormblock boot-iscsi \
    --portal "$PORTAL" --port "$PORT" \
    --iqn "$IQN" --layout "$LAYOUT" --ublk &
STORMBLOCK_PID=$!

# Wait for root device (/dev/ublkb2 — partition index 2 = root)
echo "Waiting for root device /dev/ublkb2..."
TIMEOUT=30
while [ ! -b /dev/ublkb2 ] && [ $TIMEOUT -gt 0 ]; do
    sleep 1
    TIMEOUT=$((TIMEOUT - 1))
done

if [ ! -b /dev/ublkb2 ]; then
    echo "FATAL: root device /dev/ublkb2 not found after 30s"
    echo "StormBlock PID: $STORMBLOCK_PID"
    echo "Available block devices:"
    ls -la /dev/ublk* 2>/dev/null || echo "  (none)"
    echo "Dropping to shell..."
    exec /bin/sh
fi

echo "Root device ready: /dev/ublkb2"

# Mount filesystems
echo "Mounting filesystems..."
mount -t ext4 /dev/ublkb2 /sysroot || { echo "FATAL: Failed to mount root"; exec /bin/sh; }

# Mount boot if partition exists
if [ -b /dev/ublkb1 ]; then
    mkdir -p /sysroot/boot
    mount -t ext4 /dev/ublkb1 /sysroot/boot
fi

# Mount ESP if partition exists
if [ -b /dev/ublkb0 ]; then
    mkdir -p /sysroot/boot/efi
    mount -t vfat /dev/ublkb0 /sysroot/boot/efi
fi

# Mount home if partition exists
if [ -b /dev/ublkb4 ]; then
    mkdir -p /sysroot/home
    mount -t ext4 /dev/ublkb4 /sysroot/home
fi

# Enable swap
if [ -b /dev/ublkb3 ]; then
    swapon /dev/ublkb3 2>/dev/null
fi

# Verify systemd exists in the new root
if [ ! -x /sysroot/sbin/init ] && [ ! -x /sysroot/usr/lib/systemd/systemd ]; then
    echo "FATAL: No init found in /sysroot"
    echo "Dropping to shell..."
    exec /bin/sh
fi

echo "Switching to real root..."

# Move virtual filesystems
mount --move /proc /sysroot/proc
mount --move /sys /sysroot/sys
mount --move /dev /sysroot/dev

# switch_root — PID 1 becomes /sbin/init, stormblock continues in background
exec switch_root /sysroot /sbin/init
INITSCRIPT
chmod +x "$INITRD_DIR/init"

# Build cpio archive (compressed with zstd)
echo ""
echo "Building cpio archive..."
cd "$INITRD_DIR"
find . | cpio -o -H newc --quiet 2>/dev/null | zstd -19 -T0 > "$OUTPUT"

echo ""
echo "Built: $OUTPUT"
echo "  Size: $(du -h "$OUTPUT" | cut -f1)"
echo ""
echo "Contents:"
echo "  /init                      — LinuxBoot init script"
echo "  /usr/sbin/stormblock       — $(du -h "$INITRD_DIR/usr/sbin/stormblock" | cut -f1) static binary"
echo "  /bin/busybox               — $(du -h "$INITRD_DIR/bin/busybox" | cut -f1) shell + tools"
if [ "$UBLK_FOUND" = true ]; then
    echo "  /lib/modules/ublk_drv.ko*  — kernel module"
fi
echo ""
echo "Boot kernel cmdline:"
echo "  rd.stormblock.portal=<ip> rd.stormblock.iqn=<iqn> rd.stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest"
