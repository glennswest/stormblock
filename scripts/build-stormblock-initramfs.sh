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
#   STORMBLOCK_MODULES = "virtio_scsi sd_mod erofs overlay"  (+ ublk_drv, always)
#     Override for other boot transports, e.g.
#     STORMBLOCK_MODULES="ahci sd_mod ext4" ./scripts/build-stormblock-initramfs.sh
#
# Modules are resolved with their full dependency chains (modprobe
# --show-depends) and decompressed at build time — busybox insmod cannot
# load .ko.xz, and a silently-failed storage driver surfaces later as a
# misleading "bad slab magic" (#14).
#
# Output: /tmp/stormblock-initramfs.img (zstd-compressed cpio)
#
# Requirements: busybox (static), cpio, zstd; xz/gzip for module decompression

set -euo pipefail

STORMBLOCK_BIN="${1:-target/x86_64-unknown-linux-musl/release/stormblock}"
KVER="${2:-$(uname -r)}"
OUTPUT="${3:-/tmp/stormblock-initramfs.img}"
MODULES="${STORMBLOCK_MODULES:-virtio_scsi sd_mod erofs overlay} ublk_drv"

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

# Kernel modules: resolve full dependency chains, decompress, number for
# load order. busybox insmod has no decompression and no dep resolution, so
# both must happen here at build time (#14).
copy_module() {
    # $1 = path to .ko / .ko.xz / .ko.zst / .ko.gz ; $2 = NN order prefix
    local src="$1" idx="$2" base
    base=$(basename "$src")
    base="${base%.xz}"; base="${base%.zst}"; base="${base%.gz}"
    local dst="$INITRD_DIR/lib/modules/${idx}-${base}"
    [ -f "$dst" ] && return 0
    # Skip if this module (any order prefix) is already bundled
    if ls "$INITRD_DIR/lib/modules/"*"-${base}" >/dev/null 2>&1; then
        return 0
    fi
    case "$src" in
        *.xz)  xz -dc "$src" > "$dst" ;;
        *.zst) zstd -qdc "$src" > "$dst" ;;
        *.gz)  gzip -dc "$src" > "$dst" ;;
        *)     cp "$src" "$dst" ;;
    esac
    echo "  module:     ${idx}-${base}  ($src)"
}

MOD_IDX=0
MISSING_MODS=""
for mod in $MODULES; do
    # Dep-ordered list of module paths; "builtin" lines mean nothing to load.
    deps=$(modprobe --show-depends -S "$KVER" "$mod" 2>/dev/null \
        | awk '$1 == "insmod" {print $2}') || true
    if [ -z "$deps" ]; then
        if modprobe --show-depends -S "$KVER" "$mod" 2>/dev/null | grep -q builtin; then
            echo "  module:     $mod is builtin, skipping"
        else
            MISSING_MODS="$MISSING_MODS $mod"
            echo "  WARNING: module $mod not found for kernel $KVER"
        fi
        continue
    fi
    for dep in $deps; do
        MOD_IDX=$((MOD_IDX + 1))
        copy_module "$dep" "$(printf '%02d' "$MOD_IDX")"
    done
done
if [ -n "$MISSING_MODS" ]; then
    echo "  WARNING: missing modules:$MISSING_MODS — /init will try modprobe at boot"
fi

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

# Load bundled kernel modules — decompressed and NN- ordered at build time,
# so plain insmod works and dependencies load first (#14).
for ko in /lib/modules/*.ko; do
    [ -f "$ko" ] || continue
    insmod "$ko" 2>/dev/null || true
done
# Fallback for anything the build couldn't bundle
modprobe ublk_drv 2>/dev/null || true

if [ ! -c /dev/ublk-control ]; then
    echo "WARNING: /dev/ublk-control not found — ublk_drv may not be loaded"
fi

# RHEL10 ships kernel.io_uring_disabled=2 (hardening); ublk IS io_uring,
# so re-enable it before starting the server (#14). Installed nodes must
# also persist this via /etc/sysctl.d/ — see systemd/95-stormblock-iouring.conf.
if [ -e /proc/sys/kernel/io_uring_disabled ]; then
    echo 0 > /proc/sys/kernel/io_uring_disabled
fi

# Network setup (iSCSI boot only — local-slab boot needs no network)
if [ "$BOOT_MODE" = "local" ]; then
    # Load ublk and jump straight to the local attach.
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
