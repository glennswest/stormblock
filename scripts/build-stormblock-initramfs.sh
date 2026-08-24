#!/bin/bash
# build-stormblock-initramfs.sh — Build a minimal LinuxBoot-style initramfs
#
# Creates a self-contained initramfs containing:
#   /init               — Boot init script (busybox sh)
#   /usr/sbin/stormblock — Static binary
#   /bin/busybox         — Shell + basic tools
#   /lib/modules/        — kernel modules, DECOMPRESSED, dep-ordered (#14)
#   /dev, /proc, /sys, /sysroot — mount points
#
# Usage:
#   ./scripts/build-stormblock-initramfs.sh [stormblock-binary] [kernel-version]
#
# Defaults:
#   stormblock-binary = target/x86_64-unknown-linux-musl/release/stormblock
#   kernel-version    = $(uname -r)
# Modules: **every storage and network driver**, not a list. This image is
# written to real machines and the machine decides what is in it. /init asks
# each device for the driver it names, so the same image boots a hypervisor and
# a server with an HBA it has never seen.
#
# Modules are decompressed at build time — busybox cannot read .ko.xz — and
# `depmod` builds the dependency and alias tables /init resolves against. A
# silently-failed storage driver surfaces later as a misleading "bad slab
# magic" (#14), so a depmod failure fails the build.
#
# Output: /tmp/stormblock-initramfs.img (zstd-compressed cpio)
#
# Requirements: busybox (static), cpio, zstd; xz/gzip for module decompression

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
# Every applet /init calls. A missing one is not a warning at build time and
# not an error until the line runs — `basename: not found` took a node to a
# shell with no network, at the one moment nothing is left to debug it with.
# The list is checked against the script below at build time so it cannot drift
# again.
for cmd in sh mount umount insmod modprobe ip cat grep cut sleep ln mkdir \
           swapon switch_root mdev udhcpc dmesg echo printf true false test \
           ls rm cp mv chmod chown mknod basename dirname awk sed tr head \
           tail wc mountpoint sync; do
    ln -s busybox "$INITRD_DIR/bin/$cmd"
done

# StormBlock binary
cp "$STORMBLOCK_BIN" "$INITRD_DIR/usr/sbin/stormblock"
chmod 755 "$INITRD_DIR/usr/sbin/stormblock"

# Kernel modules.
#
# **Every storage and network driver, not a list someone guessed.** This image
# is written to real machines, and the machine decides what is in it: an HBA
# from one vendor, a NIC from another, NVMe behind a switch. A hand-picked list
# boots the hypervisor it was tested on and leaves a real server sitting in an
# initramfs shell saying it has no disks — which is exactly what happened here
# with a NIC whose driver was not bundled.
#
# So the whole of drivers/{net,scsi,nvme,ata,block,virtio,usb/storage,md} comes
# along, plus the filesystems, and `depmod` builds the dependency and alias
# tables. At boot /init walks every device's `modalias` and asks for the driver
# it names — the coldplug udev would do, without udev. About 18 MB compressed,
# against a 32 GB image.
MODDIR="/lib/modules/$KVER"
DEST="$INITRD_DIR/lib/modules/$KVER"
mkdir -p "$DEST"

if [ ! -d "$MODDIR/kernel" ]; then
    echo "ERROR: no modules for kernel $KVER at $MODDIR"
    exit 1
fi

# Decompress on the way in: busybox insmod cannot read .ko.xz, and a module
# that silently fails to load surfaces later as missing hardware (#14).
copy_tree() {
    local rel="$1"
    [ -d "$MODDIR/$rel" ] || return 0
    local n=0
    while IFS= read -r src; do
        local out="$DEST/${src#$MODDIR/}"
        out="${out%.xz}"; out="${out%.zst}"; out="${out%.gz}"
        mkdir -p "$(dirname "$out")"
        case "$src" in
            *.xz)  xz -dc  "$src" > "$out" ;;
            *.zst) zstd -qdc "$src" > "$out" ;;
            *.gz)  gzip -dc "$src" > "$out" ;;
            *)     cp "$src" "$out" ;;
        esac
        n=$((n + 1))
    done <<EOF
$(find "$MODDIR/$rel" -name '*.ko*' 2>/dev/null)
EOF
    [ "$n" -gt 0 ] && printf '  modules:    %-26s %4d\n' "$rel" "$n"
    return 0
}

for tree in kernel/drivers/net kernel/drivers/scsi kernel/drivers/nvme \
            kernel/drivers/ata kernel/drivers/block kernel/drivers/virtio \
            kernel/drivers/usb/storage kernel/drivers/md kernel/drivers/messages \
            kernel/drivers/pci/controller kernel/fs kernel/lib kernel/crypto; do
    copy_tree "$tree"
done

# ublk_drv is the one module this image cannot boot without, wherever it lives.
while IFS= read -r src; do
    [ -n "$src" ] || continue
    out="$DEST/${src#$MODDIR/}"
    out="${out%.xz}"; out="${out%.zst}"; out="${out%.gz}"
    mkdir -p "$(dirname "$out")"
    case "$src" in
        *.xz)  xz -dc "$src" > "$out" ;;
        *.zst) zstd -qdc "$src" > "$out" ;;
        *)     cp "$src" "$out" ;;
    esac
    echo "  modules:    ublk_drv"
done <<EOF
$(find "$MODDIR" -name 'ublk_drv.ko*' 2>/dev/null)
EOF

# What depmod needs to know which names are already in the kernel, so /init
# does not spend the boot asking for modules that cannot be loaded because
# they are already there.
for f in modules.builtin modules.builtin.modinfo modules.order; do
    [ -f "$MODDIR/$f" ] && cp "$MODDIR/$f" "$DEST/$f"
done

# The dependency and alias tables. Without these, modprobe by modalias — which
# is the whole point — has nothing to resolve against.
if ! depmod -b "$INITRD_DIR" "$KVER" 2>/dev/null; then
    echo "ERROR: depmod failed; /init could not resolve drivers by modalias"
    exit 1
fi
echo "  modules:    $(find "$DEST" -name '*.ko' | wc -l) total, $(du -sh "$DEST" | cut -f1) uncompressed"

# Minimal /etc
cat > "$INITRD_DIR/etc/mdev.conf" << 'MDEV'
ublk[bc].* 0:0 0660
MDEV

# /init script — the LinuxBoot entry point
cat > "$INITRD_DIR/init" << 'INITSCRIPT'
#!/bin/sh
# StormBlock LinuxBoot init
#
# Two boot paths:
#   local (stormcos): rd.stormblock.slab=<dev-or-file> [rd.stormblock.meta=<dir>]
#                     [stormblock.volume=<uuid-or-name>] — or the same via a
#                     baked-in /etc/stormblock/boot.toml. Attaches the slab,
#                     exports the boot volume as /dev/ublkb0, switch_root.
#   iSCSI:            rd.stormblock.portal= rd.stormblock.iqn= rd.stormblock.layout=
#                     — provisions the partitioned boot disk over the network.

export PATH=/bin:/sbin:/usr/sbin

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts
mount -t devpts devpts /dev/pts
# tmpfs /run: overlay-root mounts live here so they survive switch_root
# via mount --move (#14).
mount -t tmpfs tmpfs /run

# Parse kernel cmdline parameters
PORTAL=""
IQN=""
LAYOUT=""
PORT="3260"
IP_CONF=""
SLAB=""
META=""
VOLUME=""
OVERLAY=""
IMAGE_STORE=""
WRITABLE=""
MOUNTS=""

for param in $(cat /proc/cmdline); do
    case "$param" in
        rd.stormblock.portal=*)      PORTAL="${param#*=}" ;;
        rd.stormblock.iqn=*)         IQN="${param#*=}" ;;
        rd.stormblock.layout=*)      LAYOUT="${param#*=}" ;;
        rd.stormblock.port=*)        PORT="${param#*=}" ;;
        rd.stormblock.slab=*)        SLAB="${param#*=}" ;;
        rd.stormblock.meta=*)        META="${param#*=}" ;;
        rd.stormblock.overlay=*)     OVERLAY="${param#*=}" ;;
        rd.stormblock.image-store=*) IMAGE_STORE="${param#*=}" ;;
        # Writable thin volumes, comma-separated name:mount pairs, e.g.
        # rd.stormblock.writable=var-...:/var,containers-...:/var/lib/containers
        rd.stormblock.writable=*)    WRITABLE="${param#*=}" ;;
        # Volumes this init mounts itself, for a PID 1 that is not systemd
        rd.stormblock.mount=*)       MOUNTS="${param#*=}" ;;
        stormblock.volume=*)         VOLUME="${param#*=}" ;;
        ip=*)                        IP_CONF="${param#*=}" ;;
    esac
done

# Local-slab boot (stormcos) when a slab is named on the cmdline, or when the
# initramfs carries a boot.toml handoff and no iSCSI portal was given.
BOOT_MODE="iscsi"
if [ -n "$SLAB" ]; then
    BOOT_MODE="local"
elif [ -z "$PORTAL" ] && [ -f /etc/stormblock/boot.toml ] && [ -f /etc/stormblock/slab ]; then
    # /etc/stormblock/slab: one line naming the slab device/file
    SLAB=$(cat /etc/stormblock/slab)
    BOOT_MODE="local"
fi

# Validate required parameters
if [ "$BOOT_MODE" = "iscsi" ] && { [ -z "$PORTAL" ] || [ -z "$IQN" ] || [ -z "$LAYOUT" ]; }; then
    echo "FATAL: Missing required kernel parameters:"
    echo "  rd.stormblock.portal=$PORTAL"
    echo "  rd.stormblock.iqn=$IQN"
    echo "  rd.stormblock.layout=$LAYOUT"
    echo "  (or rd.stormblock.slab=<dev> for local-slab boot)"
    echo "Dropping to shell..."
    exec /bin/sh
fi

echo "StormBlock LinuxBoot init ($BOOT_MODE)"
if [ "$BOOT_MODE" = "local" ]; then
    echo "  Slab:   $SLAB"
    [ -n "$META" ] && echo "  Meta:   $META"
    [ -n "$VOLUME" ] && echo "  Volume: $VOLUME"
else
    echo "  Portal: $PORTAL:$PORT"
    echo "  IQN:    $IQN"
    echo "  Layout: $LAYOUT"
fi

# Load the drivers this machine actually needs.
#
# Every device on every bus names the driver it wants in its `modalias`, and
# the initramfs carries the whole of the storage and network trees with the
# alias table `depmod` built. So this asks the hardware rather than being told
# — the coldplug udev does, without udev — and the same image boots a
# hypervisor, a server with an HBA and a server with NVMe behind a switch.
#
# Failures are silent on purpose: most devices on a bus are not storage or
# network, and their drivers are deliberately not here.
echo "Loading drivers for this machine..."
KVER=$(uname -r)
LOADED=0
if [ -f "/lib/modules/$KVER/modules.dep" ]; then
    for ma in /sys/bus/*/devices/*/modalias; do
        [ -f "$ma" ] || continue
        if modprobe -q "$(cat "$ma")" 2>/dev/null; then
            LOADED=$((LOADED + 1))
        fi
    done
else
    echo "  WARNING: no modules.dep — drivers cannot be resolved by modalias"
fi

# The ones no device announces: filesystems, and the block driver this image
# exports its root through.
for m in ublk_drv erofs overlay ext4 xfs vfat; do
    modprobe -q "$m" 2>/dev/null || true
done
echo "  loaded $LOADED driver(s) by modalias"

# Give the buses a moment to present what those drivers found. A disk that
# appears half a second after its HBA is normal, and the wait below for the
# slab device covers the rest.
sleep 1

if [ ! -c /dev/ublk-control ]; then
    echo "WARNING: /dev/ublk-control not found — ublk_drv may not be loaded"
fi

# RHEL10 ships kernel.io_uring_disabled=2 (hardening); ublk IS io_uring,
# so re-enable it before starting the server (#14). Installed nodes must
# also persist this via /etc/sysctl.d/ — see systemd/95-stormblock-iouring.conf.
if [ -e /proc/sys/kernel/io_uring_disabled ]; then
    echo 0 > /proc/sys/kernel/io_uring_disabled
fi

# Network setup.
#
# An iSCSI boot cannot proceed without it — the root is across it. A local boot
# does not *need* it to reach its root, which is why this used to be skipped
# entirely; but the node it hands over to does. Nothing after switch_root
# configures an interface: stormpump is PID 1 and starts containers, and a
# container on host networking inherits whatever the host has, which was
# nothing. The symptom is every service coming up healthy and unreachable —
# "Network unreachable" from a registry talking to an engine one process away.
#
# So a local boot brings the network up too, when the command line asks for it
# with `ip=dhcp` or `ip=<addr>::<gw>:<mask>::<iface>:none`. Without `ip=` it
# stays as it was: loopback only, and nothing waits on DHCP that did not ask.
if [ "$BOOT_MODE" = "local" ] && [ -z "$IP_CONF" ]; then
    # No network asked for. Load ublk and jump straight to the local attach.
    ip link set lo up 2>/dev/null || true
    :
else
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
    if [ "$BOOT_MODE" = "local" ]; then
        # The root is on this disk; the network is for what runs later. A node
        # that boots without an address is degraded and can be looked at. One
        # that drops to a shell in the initramfs cannot be looked at at all.
        echo "WARNING: no network interface found — continuing without one"
        echo "         (is the NIC's driver in STORMBLOCK_MODULES?)"
        NO_NETWORK=1
    else
        echo "FATAL: No network interface found"
        exec /bin/sh
    fi
fi
if [ -z "${NO_NETWORK:-}" ]; then

ip link set "$IFACE" up

if [ -n "$IP_CONF" ] && [ "$IP_CONF" != "dhcp" ]; then
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
fi
fi

# Start stormblock with ublk export
echo "Starting StormBlock..."
if [ "$BOOT_MODE" = "local" ]; then
    # The slab device appears asynchronously after its driver loads — wait
    # bounded instead of letting boot-local open a nonexistent path (#14).
    if [ ! -e "$SLAB" ]; then
        echo "Waiting for slab device $SLAB..."
        TIMEOUT=30
        while [ ! -e "$SLAB" ] && [ $TIMEOUT -gt 0 ]; do
            sleep 1
            TIMEOUT=$((TIMEOUT - 1))
        done
    fi
    if [ ! -e "$SLAB" ]; then
        echo "FATAL: slab device $SLAB never appeared (storage driver missing?)"
        echo "Loaded modules:"; cat /proc/modules 2>/dev/null | cut -d' ' -f1
        echo "Dropping to shell..."
        exec /bin/sh
    fi

    # Writable thin volumes: each becomes a --writable to boot-local, exported
    # at the next ublk index after root (0) and image-store (1 if present).
    # Build the arg list and the device->mount map in the SAME order so indices
    # line up deterministically.
    WR_ARGS=""
    WR_IDX=1
    [ -n "$IMAGE_STORE" ] && WR_IDX=2
    WRITABLE_MAP=""
    if [ -n "$WRITABLE" ]; then
        OIFS=$IFS; IFS=,
        for entry in $WRITABLE; do
            IFS=$OIFS
            wname="${entry%%:*}"
            wmnt="${entry#*:}"
            [ -z "$wname" ] && { IFS=,; continue; }
            [ "$wname" = "$entry" ] && wmnt=""   # no ':' -> no mount hint
            WR_ARGS="$WR_ARGS --writable $wname"
            [ -n "$wmnt" ] && WRITABLE_MAP="$WRITABLE_MAP/dev/ublkb$WR_IDX $wmnt
"
            WR_IDX=$((WR_IDX + 1))
            IFS=,
        done
        IFS=$OIFS
    fi

    # Volumes this init **mounts**, rather than leaving to the real root's
    # init. `rd.stormblock.writable=` writes fstab, which only helps a node
    # whose PID 1 is systemd; a stormpump node never reads it, and its boot
    # manifest registers *directories* — so a container's volume has to be
    # mounted before PID 1 starts or there is nothing for it to chroot into.
    #
    #   rd.stormblock.mount=stormblock:/pallets/stormblock,fedora:/pallets/fedora
    #
    # Same ublk numbering as the writables, continuing after them, and the map
    # is built in the same pass so the indices cannot drift apart.
    MOUNT_MAP=""
    if [ -n "$MOUNTS" ]; then
        OIFS=$IFS; IFS=,
        for entry in $MOUNTS; do
            IFS=$OIFS
            mname="${entry%%:*}"
            mmnt="${entry#*:}"
            if [ -z "$mname" ] || [ "$mname" = "$entry" ]; then
                echo "  WARNING: rd.stormblock.mount entry '$entry' has no :<path> — ignored"
                IFS=,; continue
            fi
            WR_ARGS="$WR_ARGS --writable $mname"
            MOUNT_MAP="$MOUNT_MAP/dev/ublkb$WR_IDX $mmnt
"
            WR_IDX=$((WR_IDX + 1))
            IFS=,
        done
        IFS=$OIFS
    fi

    # Attach the existing slab (no reformat), export boot volume as ublkb0.
    # Volume comes from --volume if given, else /etc/stormblock/boot.toml.
    # shellcheck disable=SC2086
    /usr/sbin/stormblock boot-local \
        --slab "$SLAB" \
        ${META:+--meta "$META"} \
        ${IMAGE_STORE:+--image-store "$IMAGE_STORE"} \
        ${VOLUME:+--volume "$VOLUME"} \
        $WR_ARGS &
    ROOTDEV=/dev/ublkb0
else
    /usr/sbin/stormblock boot-iscsi \
        --portal "$PORTAL" --port "$PORT" \
        --iqn "$IQN" --layout "$LAYOUT" --ublk &
    ROOTDEV=/dev/ublkb2   # partition index 2 = root
fi
STORMBLOCK_PID=$!

echo "Waiting for root device $ROOTDEV..."
TIMEOUT=30
while [ ! -b "$ROOTDEV" ] && [ $TIMEOUT -gt 0 ]; do
    sleep 1
    TIMEOUT=$((TIMEOUT - 1))
done

if [ ! -b "$ROOTDEV" ]; then
    echo "FATAL: root device $ROOTDEV not found after 30s"
    echo "StormBlock PID: $STORMBLOCK_PID"
    echo "Available block devices:"
    ls -la /dev/ublk* 2>/dev/null || echo "  (none)"
    echo "Dropping to shell..."
    exec /bin/sh
fi

echo "Root device ready: $ROOTDEV"

# Mount filesystems (stormcos local root is erofs; fall back to ext4/auto)
echo "Mounting filesystems..."
mount_root() {
    # $1 = device, $2 = mountpoint
    mount -t erofs -o ro "$1" "$2" 2>/dev/null \
        || mount -t ext4 "$1" "$2" 2>/dev/null \
        || mount "$1" "$2"
}

if [ -n "$OVERLAY" ]; then
    # Immutable-OS mode (#14): read-only root as overlay lowerdir, writable
    # upper on tmpfs or a block device.
    #   rd.stormblock.overlay=tmpfs[:SIZE]   e.g. tmpfs:1G (default 512m)
    #   rd.stormblock.overlay=/dev/ublkb1    pre-formatted writable volume
    echo "Overlay root: lower=$ROOTDEV upper=$OVERLAY"
    mkdir -p /run/stormblock/lower /run/stormblock/rw
    mount_root "$ROOTDEV" /run/stormblock/lower \
        || { echo "FATAL: Failed to mount overlay lower"; exec /bin/sh; }

    case "$OVERLAY" in
        tmpfs|tmpfs:*)
            SIZE="${OVERLAY#tmpfs}"; SIZE="${SIZE#:}"
            mount -t tmpfs -o "size=${SIZE:-512m}" tmpfs /run/stormblock/rw \
                || { echo "FATAL: Failed to mount overlay tmpfs"; exec /bin/sh; }
            ;;
        *)
            TIMEOUT=15
            while [ ! -b "$OVERLAY" ] && [ $TIMEOUT -gt 0 ]; do
                sleep 1; TIMEOUT=$((TIMEOUT - 1))
            done
            mount "$OVERLAY" /run/stormblock/rw \
                || { echo "FATAL: Failed to mount overlay upper $OVERLAY"; exec /bin/sh; }
            ;;
    esac
    mkdir -p /run/stormblock/rw/upper /run/stormblock/rw/work
    mount -t overlay overlay \
        -o lowerdir=/run/stormblock/lower,upperdir=/run/stormblock/rw/upper,workdir=/run/stormblock/rw/work \
        /sysroot \
        || { echo "FATAL: Failed to mount overlay root"; exec /bin/sh; }
else
    mount_root "$ROOTDEV" /sysroot \
        || { echo "FATAL: Failed to mount root"; exec /bin/sh; }
fi

if [ "$BOOT_MODE" = "iscsi" ]; then
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
fi

# Writable thin volumes (var, containers): boot-local exported them as ublk
# devices after root. We can't mkfs.xfs here (busybox has no mkfs.xfs), so hand
# them to systemd via fstab in the real root — x-systemd.makefs formats the
# empty volume on first boot, x-systemd.growfs grows the fs after auto-expand,
# and the mounts land over the read-only erofs root. Writing /sysroot/etc/fstab
# copies-up into the overlay upper (regenerated every boot, which is fine).
# Mount what this init was told to mount, into the real root, before PID 1
# starts. A stormpump node's boot manifest registers directories, so a
# container's volume has to be a mounted directory by the time PID 1 reads the
# manifest — there is no later moment, and nothing else on the node will do it.
#
# A volume that will not mount is reported and skipped rather than fatal: one
# container that cannot start is worth less than a node that does not boot, and
# the supervisor says which one is missing.
if [ -n "$MOUNT_MAP" ]; then
    echo "Mounting container volumes..."
    printf '%s' "$MOUNT_MAP" | while read -r mdev mmnt; do
        [ -z "$mdev" ] && continue
        n=0
        while [ ! -b "$mdev" ] && [ $n -lt 15 ]; do sleep 1; n=$((n + 1)); done
        if [ ! -b "$mdev" ]; then
            echo "  WARNING: $mdev never appeared; $mmnt will be empty"
            continue
        fi
        mkdir -p "/sysroot$mmnt"
        if mount "$mdev" "/sysroot$mmnt" 2>/dev/null; then
            echo "  mounted: $mdev -> $mmnt"
        else
            echo "  WARNING: $mdev would not mount at $mmnt"
        fi
    done
fi

if [ -n "$WRITABLE_MAP" ]; then
    echo "Registering writable thin volumes in fstab..."
    printf '%s' "$WRITABLE_MAP" | while read -r wdev wmnt; do
        [ -z "$wdev" ] && continue
        n=0
        while [ ! -b "$wdev" ] && [ $n -lt 15 ]; do sleep 1; n=$((n + 1)); done
        if [ -b "$wdev" ]; then
            echo "$wdev $wmnt xfs defaults,x-systemd.makefs,x-systemd.growfs,nofail 0 0" \
                >> /sysroot/etc/fstab
            echo "  writable: $wdev -> $wmnt"
        else
            echo "  WARNING: $wdev never appeared; $wmnt falls back to overlay (ephemeral)"
        fi
    done
fi

# Preloaded image store: boot-local exported it as ublkb1 (right after root).
# It is an erofs filesystem image, mounted READ-ONLY — it is the build-time
# preload that CRI-O/rspacefs serve from, so a zeroboot node never has to pull.
# Registered in fstab like the writable volumes so systemd owns the mount
# ordering (it sits under /var, which must mount first).
if [ -n "$IMAGE_STORE" ]; then
    ISDEV=/dev/ublkb1
    ISMNT="${IMAGE_STORE_MOUNT:-/var/lib/stormcos/image-store}"
    n=0
    while [ ! -b "$ISDEV" ] && [ $n -lt 15 ]; do sleep 1; n=$((n + 1)); done
    if [ -b "$ISDEV" ]; then
        mkdir -p "/sysroot$ISMNT"
        echo "$ISDEV $ISMNT erofs ro,nofail 0 0" >> /sysroot/etc/fstab
        echo "  image-store: $ISDEV -> $ISMNT (ro)"
    else
        echo "  WARNING: $ISDEV never appeared — preloaded images will NOT be available"
    fi
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
# Carry /run (holds the overlay lower/upper mounts) into the new root
mkdir -p /sysroot/run
mount --move /run /sysroot/run 2>/dev/null || true

# switch_root — PID 1 becomes /sbin/init, stormblock continues in background
exec switch_root /sysroot /sbin/init
INITSCRIPT
chmod +x "$INITRD_DIR/init"

# The applets /init actually calls, against the ones bundled. A command that
# is not there fails at the line that needs it — deep in a boot, with no
# network and nothing to look at but a console.
MISSING_APPLETS=""
for cmd in $(grep -oE '\b(basename|dirname|awk|sed|tr|head|tail|wc|mountpoint|sync|blkid|readlink|stat|seq|find|xargs|expr|sort|uniq)\b' \
        "$INITRD_DIR/init" | sort -u); do
    [ -e "$INITRD_DIR/bin/$cmd" ] || MISSING_APPLETS="$MISSING_APPLETS $cmd"
done
if [ -n "$MISSING_APPLETS" ]; then
    echo "ERROR: /init calls applets this initramfs does not carry:$MISSING_APPLETS"
    echo "       add them to the symlink list above"
    exit 1
fi

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
if ls "$INITRD_DIR/lib/modules/"*ublk_drv* >/dev/null 2>&1; then
    echo "  /lib/modules/*ublk_drv*    — kernel module"
fi
echo ""
echo "Boot kernel cmdline:"
echo "  iSCSI: rd.stormblock.portal=<ip> rd.stormblock.iqn=<iqn> rd.stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest"
echo "  local: root=/dev/ublkb0 rd.stormblock.slab=<dev-or-file> [rd.stormblock.meta=<dir>] [stormblock.volume=<uuid-or-name>]"
echo "         [rd.stormblock.overlay=tmpfs[:SIZE]|<blockdev>]  — writable overlay over a read-only (erofs) root"
echo "         [rd.stormblock.mount=<vol>:<path>,...]  — export and MOUNT these into the real root"
echo "                 for a PID 1 that is not systemd (stormpump reads directories, not fstab)"
