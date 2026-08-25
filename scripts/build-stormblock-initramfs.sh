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

# Busybox (static) + a symlink for **every applet it has**.
#
# Not a list. A list is a guess about what /init will need, maintained by
# whoever remembers to update it, and it fails at the worst moment: the applet
# is missing, the shell says "not found", and the node sits in an initramfs
# with no network and no disks. That happened twice here — `basename`, then
# `uname` — and the second time the *checker* for the list had the same bug as
# the list.
#
# busybox knows exactly what it can do. Asking it costs a few hundred symlinks,
# which is about 50 KB in the archive, and removes the question permanently.
cp "$BUSYBOX" "$INITRD_DIR/bin/busybox"
chmod 755 "$INITRD_DIR/bin/busybox"
APPLETS=0
for cmd in $("$BUSYBOX" --list); do
    # `busybox` itself is the real binary, not a link to itself.
    [ "$cmd" = "busybox" ] && continue
    ln -sf busybox "$INITRD_DIR/bin/$cmd"
    APPLETS=$((APPLETS + 1))
done
echo "  applets:    $APPLETS (everything this busybox provides)"

# The real modprobe, with its libraries.
#
# busybox has one, and it is not the one a distro uses. Everything else in
# here is busybox on purpose, but module loading is where the initramfs earns
# its keep: it has to resolve an alias like
# `virtio:d00000001v00001AF4` through modules.alias, follow modules.dep, and
# decompress whatever the module is compressed with. kmod is what every distro
# trusts to do that, and this image is written to machines whose hardware we
# have never seen.
#
# It is dynamically linked, so its libraries and the loader come too — about
# 5 MB, against 27 MB of drivers those libraries exist to load.
KMOD="$(command -v modprobe || echo /usr/sbin/modprobe)"
if [ -x "$KMOD" ]; then
    mkdir -p "$INITRD_DIR/usr/sbin" "$INITRD_DIR/lib64"
    cp -L "$KMOD" "$INITRD_DIR/usr/sbin/modprobe"
    # depmod is the same binary; carry it under its own name so a rebuild of
    # the tables is possible from inside a running node.
    cp -L "$KMOD" "$INITRD_DIR/usr/sbin/depmod"
    for lib in $(ldd "$KMOD" | grep -oE '/[^ ]+\.so[^ ]*'); do
        cp -L "$lib" "$INITRD_DIR/lib64/" 2>/dev/null || true
    done
    cp -L /lib64/ld-linux-x86-64.so.2 "$INITRD_DIR/lib64/" 2>/dev/null || true
    echo "  modprobe:   kmod $("$KMOD" --version 2>/dev/null | head -1 | awk '{print $NF}')"
else
    # Not a warning. The module tree ships compressed, and busybox's insmod
    # cannot read a compressed module — it would fail on every driver, one at
    # a time, silently, and surface as hardware that does not exist.
    echo "ERROR: no kmod modprobe on this build host."
    echo "       The module tree is bundled compressed, as the kernel package"
    echo "       ships it, and only kmod can load that. Install kmod."
    exit 1
fi

# udev, and the rules it runs on.
#
# **This is not a rescue shell, it is a node.** Device discovery on hardware
# nobody has seen is exactly the problem udev exists to solve, and every
# distro that boots on arbitrary machines ships it in the initramfs. Walking
# /sys and calling modprobe by hand gets the easy half and then quietly misses
# a NIC — which is what happened here, twice, before this.
#
# udevd handles the ordering, the retries, the buses that only appear once
# their parent's driver has bound, and the device nodes. What was a hand-rolled
# sweep with a settle loop becomes `udevadm trigger` and `udevadm settle`, run
# by the code every other distro runs.
# `ls` returns non-zero for the paths that do not exist, and with `pipefail`
# that fails the assignment and `set -e` ends the build — silently, because
# the failing command printed nothing. Hence the `|| true`.
UDEVD=""
for cand in /usr/lib/systemd/systemd-udevd /lib/systemd/systemd-udevd /sbin/udevd; do
    [ -x "$cand" ] && { UDEVD="$cand"; break; }
done
UDEVADM="$(command -v udevadm || true)"
if [ -x "$UDEVD" ] && [ -x "$UDEVADM" ]; then
    mkdir -p "$INITRD_DIR/usr/lib/systemd" "$INITRD_DIR/usr/bin" \
             "$INITRD_DIR/usr/lib/udev/rules.d" "$INITRD_DIR/run/udev"
    cp -L "$UDEVADM" "$INITRD_DIR/usr/bin/udevadm"
    cp -L "$UDEVD"   "$INITRD_DIR/usr/lib/systemd/systemd-udevd"
    for b in "$UDEVADM" "$UDEVD"; do
        for lib in $(ldd "$b" 2>/dev/null | grep -oE '/[^ ]+\.so[^ ]*'); do
            cp -L "$lib" "$INITRD_DIR/lib64/" 2>/dev/null || true
        done
    done
    # The rules, and the helpers they invoke. Rules that call a helper which is
    # not there fail silently, which is the worst way for this to go wrong.
    cp -a /usr/lib/udev/rules.d/. "$INITRD_DIR/usr/lib/udev/rules.d/" 2>/dev/null || true
    for h in /usr/lib/udev/*_id /usr/lib/udev/*-id /usr/lib/udev/mtd_probe; do
        [ -x "$h" ] || continue
        cp -L "$h" "$INITRD_DIR/usr/lib/udev/" 2>/dev/null || true
        for lib in $(ldd "$h" 2>/dev/null | grep -oE '/[^ ]+\.so[^ ]*'); do
            cp -L "$lib" "$INITRD_DIR/lib64/" 2>/dev/null || true
        done
    done
    echo "  udev:       $("$UDEVADM" --version 2>/dev/null | head -1), $(ls "$INITRD_DIR/usr/lib/udev/rules.d" | wc -l) rules"
else
    echo "  WARNING: no udevd found; /init will fall back to walking modalias"
fi

# Firmware — what a *server* needs to reach its root and its network.
#
# A NIC or HBA that loads firmware at probe fails without it, and the failure
# is indistinguishable from a missing driver: the device simply never appears.
# Mellanox, Broadcom, several Intel parts and most FC HBAs are in that group,
# which is most of what a real server has in it. So this cannot be a guessed
# list of models.
#
# It can, though, be a statement about what a storage node *is not*. The full
# set is 310 MB, and 215 MB of that is GPU, WiFi, Bluetooth and phone-SoC
# firmware — 103 MB of nvidia, 32 MB of mediatek, 27 MB of amdgpu, 35 MB of
# ath11k/ath12k — on a machine that has none of it and never will. Excluding
# those classes leaves 49 MB and removes nothing a server can use.
#
# The complete set still ships, in the kernel pallet's `modules` golden, and is
# mounted over this one once the root filesystem is up. So a machine that turns
# out to have an exotic device is not stuck: it is only missing the firmware
# for the few seconds before its root is mounted, and the only devices that
# matter in that window are the disk it boots from and the NIC it may DHCP on.
FWDIR="$(ls -d /usr/lib/firmware /lib/firmware 2>/dev/null | head -1 || true)"
# Categories a storage node does not have. Named by what they are rather than
# by which models exist, because the model list changes every release and the
# category list does not.
FW_EXCLUDE="nvidia amdgpu radeon i915 xe mediatek ath9k_htc ath10k ath11k ath12k
            ath6k ar3k brcm cypress rtw88 rtw89 rtlwifi iwlwifi cirrus qca
            ti-connectivity mrvl libertas mwl8k rsi wfx av7110 cpia2 as102
            ene-ubox go7007 s2250 s5p-mfc vpu mt76 rockchip amlogic sun8i-a83t
            meson tegra"
if [ -n "$FWDIR" ] && [ -d "$FWDIR" ]; then
    mkdir -p "$INITRD_DIR/lib/firmware"
    cp -a "$FWDIR/." "$INITRD_DIR/lib/firmware/" 2>/dev/null || true
    for d in $FW_EXCLUDE; do
        rm -rf "$INITRD_DIR/lib/firmware/$d" 2>/dev/null || true
    done
    echo "  firmware:   $(du -sh "$INITRD_DIR/lib/firmware" | cut -f1) (of $(du -sh "$FWDIR" | cut -f1); the rest is in the modules golden)"
else
    echo "  WARNING: no linux-firmware on this build host — devices that load"
    echo "           firmware at probe will look like missing drivers"
fi

# StormBlock binary
cp "$STORMBLOCK_BIN" "$INITRD_DIR/usr/sbin/stormblock"
chmod 755 "$INITRD_DIR/usr/sbin/stormblock"

# Kernel modules — what is needed to reach the root, and nothing else.
#
# The initramfs is loaded into RAM in full on every boot, so everything in it
# is paid for every time by every node. What it actually needs is narrow: the
# drivers that reach the disk this node boots from, and the drivers for the NIC
# it may DHCP on. Everything else — the whole 73 MB tree — is in the kernel
# pallet's `modules` golden and is bound over /lib/modules the moment the root
# is up, which is before any of it is wanted.
#
# Chosen by *purpose*, not by model. Whole subtrees, so there is no list of
# drivers to keep current and no chance of missing the one card this machine
# has: every storage driver and every network driver, not a selection of them.
#
# The dependency closure below is what makes that safe. A driver's dependencies
# are not confined to its own subtree — `net_failover` links against
# `kernel/net/core/failover.ko`, which is under no driver directory at all —
# and a missing one is silent: modprobe loads what it can, the kernel refuses
# it on unresolved symbols, and the driver that needed it never appears. So
# rather than guess which extra directories to add, ask depmod and copy what it
# names, until it names nothing that is not here.
MODDIR="/lib/modules/$KVER"
DEST="$INITRD_DIR/lib/modules/$KVER"
mkdir -p "$DEST"

if [ ! -d "$MODDIR/kernel" ]; then
    echo "ERROR: no modules for kernel $KVER at $MODDIR"
    exit 1
fi

# Reaching the root, and being reachable.
for tree in \
    kernel/drivers/scsi kernel/drivers/nvme kernel/drivers/ata \
    kernel/drivers/block kernel/drivers/virtio kernel/drivers/md \
    kernel/drivers/usb/storage kernel/drivers/message \
    kernel/drivers/pci/controller kernel/drivers/nvdimm \
    kernel/drivers/net \
    kernel/fs kernel/lib kernel/crypto
do
    [ -d "$MODDIR/$tree" ] || continue
    mkdir -p "$DEST/$(dirname "$tree")"
    cp -a "$MODDIR/$tree" "$DEST/$(dirname "$tree")/"
done

for f in modules.builtin modules.builtin.modinfo modules.order; do
    [ -f "$MODDIR/$f" ] && cp "$MODDIR/$f" "$DEST/$f"
done

# Close the dependency set over the **source** tree's map, not this one's.
#
# This is subtle and it has already cost a boot twice. `depmod` records
# dependencies only between modules it can *see*: run it against a subset that
# is missing `failover.ko` and it does not report that `net_failover` needs it
# — it cannot name a file that is not there — so the generated modules.dep is
# self-consistent, complete-looking, and wrong. A closure computed from it adds
# nothing, and the node boots with no network because virtio_net's dependency
# never loaded.
#
# The full tree's modules.dep knows the real graph. Seed it with what was
# selected above and take the transitive closure there, then copy the result.
FULL_DEP="$MODDIR/modules.dep"
if [ ! -f "$FULL_DEP" ]; then
    echo "ERROR: $FULL_DEP is missing — cannot close the dependency set"
    exit 1
fi

SEED=$(mktemp)
( cd "$DEST" && find kernel -name '*.ko*' 2>/dev/null ) | sed "s|^|kernel/|;s|^kernel/kernel/|kernel/|" > "$SEED"

WANT=$(mktemp)
awk -F: '
    NR == FNR { gsub(/^[ \t]+/, "", $2); deps[$1] = $2; next }
    { want[$0] = 1 }
    END {
        changed = 1
        while (changed) {
            changed = 0
            for (m in want) {
                n = split(deps[m], d, " ")
                for (i = 1; i <= n; i++) {
                    if (!(d[i] in want)) { want[d[i]] = 1; changed = 1 }
                }
            }
        }
        for (m in want) print m
    }
' "$FULL_DEP" "$SEED" > "$WANT"

pulled=0
while IFS= read -r dep; do
    [ -n "$dep" ] || continue
    [ -f "$DEST/$dep" ] && continue
    if [ -f "$MODDIR/$dep" ]; then
        mkdir -p "$DEST/$(dirname "$dep")"
        cp -a "$MODDIR/$dep" "$DEST/$dep"
        pulled=$((pulled + 1))
    else
        echo "ERROR: $dep is required but is not in $MODDIR either"
        exit 1
    fi
done < "$WANT"
rm -f "$SEED" "$WANT"
[ "$pulled" -gt 0 ] && echo "  modules:    $pulled dependency module(s) pulled in from outside the chosen trees"

if ! depmod -b "$INITRD_DIR" "$KVER" 2>/dev/null; then
    echo "ERROR: depmod failed; /init could not resolve drivers by modalias"
    exit 1
fi

# And prove it: every module the *source* tree says is needed must be here.
# A tree that is complete against its own depmod can still be missing what it
# needs, which is the whole reason this check reads the full map.
missing=0
while IFS= read -r m; do
    [ -n "$m" ] || continue
    [ -f "$DEST/$m" ] || { echo "  MISSING dependency: $m"; missing=$((missing + 1)); }
done <<EOF
$(awk -F: -v d="$DEST" '
    { gsub(/^[ \t]+/, "", $2)
      if (system("test -f " d "/" $1) == 0) { n = split($2, x, " "); for (i=1;i<=n;i++) print x[i] } }
' "$FULL_DEP" | sort -u)
EOF
if [ "$missing" -gt 0 ]; then
    echo "ERROR: $missing module(s) required by what this image carries are absent."
    exit 1
fi

printf '  modules:    %d files, %s (of %s; the rest is in the modules golden)\n' \
    "$(find "$DEST" -name '*.ko*' | wc -l)" \
    "$(du -sh "$DEST" | cut -f1)" \
    "$(du -sh "$MODDIR/kernel" | cut -f1)"

# What depmod needs to know which names are already in the kernel, so /init
# does not spend the boot asking for modules that cannot be loaded because
# they are already there.
for f in modules.builtin modules.builtin.modinfo modules.order; do
    [ -f "$MODDIR/$f" ] && cp "$MODDIR/$f" "$DEST/$f"
done


# The DHCP client's script.
#
# udhcpc configures nothing itself — it obtains a lease and execs a script with
# the values in the environment. Without one, a node takes an address from the
# server (which then shows in the server's lease table, looking exactly like
# success) and never puts it on the interface.
mkdir -p "$INITRD_DIR/usr/share/udhcpc"
cat > "$INITRD_DIR/usr/share/udhcpc/default.script" << 'DHCPSCRIPT'
#!/bin/sh
# Apply what the DHCP server offered. Called by udhcpc with $1 as the reason
# and the lease in the environment.
case "$1" in
    bound|renew)
        ip addr flush dev "$interface" 2>/dev/null
        ip addr add "$ip/${mask:-24}" dev "$interface"
        ip link set "$interface" up
        # Only the first router: a node with two default routes has one it did
        # not choose, and the failure is intermittent.
        for r in $router; do
            ip route add default via "$r" dev "$interface" 2>/dev/null && break
        done
        : > /etc/resolv.conf
        [ -n "$domain" ] && echo "search $domain" >> /etc/resolv.conf
        for d in $dns; do
            echo "nameserver $d" >> /etc/resolv.conf
        done
        # The lease usually carries NTP servers (DHCP option 42). Written down
        # rather than used here, because the clock is set once, after the
        # network is up, and not on every renewal.
        [ -n "$ntpsrv" ] && echo "$ntpsrv" > /run/ntp-servers
        # DHCP option 12, if the server has an opinion about what this machine
        # is called. It usually knows better than the machine does.
        [ -n "$hostname" ] && echo "$hostname" > /run/dhcp-hostname
        ;;
    deconfig)
        ip addr flush dev "$interface" 2>/dev/null
        ip link set "$interface" up
        ;;
esac
exit 0
DHCPSCRIPT
chmod 755 "$INITRD_DIR/usr/share/udhcpc/default.script"

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

# /usr/sbin first: the real modprobe lives there and busybox's link to itself
# is in /bin. Which one resolves an alias is the difference between a node
# that finds its hardware and one that reports having none.
export PATH=/usr/sbin:/bin:/sbin

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

# Where the boot time goes.
#
# A number nobody can break down is a number nobody can improve. Each stage
# prints what it cost and where it sits in the boot, from /proc/uptime — which
# starts at kernel entry, so the first stamp also says what firmware and the
# kernel spent before this script existed.
#
# Permanent, not instrumentation added for one investigation: the cost of a
# boot changes when a driver is added or a golden grows, and the only way that
# is noticed is if every boot says so.
T_LAST=0
stamp() {
    read -r up _ < /proc/uptime
    echo "  [+$(awk -v a="$up" -v b="$T_LAST" 'BEGIN{printf "%.1f", a-b}')s | ${up}s] $*"
    T_LAST="$up"
}
stamp "kernel handed over (firmware + kernel init before this)"
if [ "$BOOT_MODE" = "local" ]; then
    echo "  Slab:   $SLAB"
    [ -n "$META" ] && echo "  Meta:   $META"
    [ -n "$VOLUME" ] && echo "  Volume: $VOLUME"
else
    echo "  Portal: $PORTAL:$PORT"
    echo "  IQN:    $IQN"
    echo "  Layout: $LAYOUT"
fi

# Find the hardware.
#
# Two mechanisms, deliberately, because they fail differently. The walk is
# deterministic and needs no daemon: every device names the driver it wants in
# its modalias, and kmod resolves that through modules.alias — verified to
# turn `virtio:d00000001v00001AF4` into net_failover + virtio_net. It repeats
# until nothing new loads, because a bus driver creates the devices behind it
# and one sweep finds the bridge and misses what is across it.
#
# udev runs after, for what rules do beyond loading modules and for anything
# the walk did not reach. Its errors are *not* hidden: a daemon that fails to
# start silently is how this came to load nothing at all while reporting
# success.
echo "Discovering hardware..."
MODTREE=""
for d in /lib/modules/*/; do
    [ -f "$d/modules.dep" ] && MODTREE="$d"
done
LOADED=0
if [ -n "$MODTREE" ]; then
    PASS=0
    while [ $PASS -lt 5 ]; do
        PASS=$((PASS + 1)); FOUND=0
        for ma in /sys/bus/*/devices/*/modalias; do
            [ -f "$ma" ] || continue
            modprobe -q "$(cat "$ma")" 2>/dev/null && FOUND=$((FOUND + 1))
        done
        LOADED=$((LOADED + FOUND))
        [ "$FOUND" -eq 0 ] && break
        sleep 1
    done
    echo "  $LOADED driver(s) loaded in $PASS pass(es)"
    stamp "drivers discovered"

    # A module the kernel *rejected* is not a module that failed to match, and
    # the difference matters: the first means the driver is here and broken,
    # the second means this machine does not need it. modprobe reports both the
    # same way, so ask the kernel instead. This is how a missing dependency
    # announced itself for a whole boot while discovery reported success.
    REJECTED="$(dmesg 2>/dev/null | grep -c 'Unknown symbol' || true)"
    if [ "${REJECTED:-0}" -gt 0 ]; then
        echo "  WARNING: the kernel rejected $REJECTED module load(s) on unresolved"
        echo "           symbols — a driver is present but its dependency is not:"
        dmesg | grep 'Unknown symbol' | sed 's/^/    /' | head -5
    fi
else
    echo "  WARNING: no module tree found under /lib/modules"
fi

mkdir -p /run/udev
if [ -x /usr/lib/systemd/systemd-udevd ] && [ -x /usr/bin/udevadm ]; then
    /usr/lib/systemd/systemd-udevd --daemon
    udevadm trigger --type=subsystems --action=add
    udevadm trigger --type=devices --action=add
    udevadm settle --timeout=30
    echo "  udev settled; $(lsmod 2>/dev/null | tail -n +2 | wc -l) module(s) now loaded"
    stamp "udev settled"
else
    echo "  udev not present in this image"
fi

# The ones no device announces: filesystems, and the block driver this image
# exports its root through.
for m in ublk_drv erofs overlay ext4 xfs vfat; do
    modprobe -q "$m" 2>/dev/null || true
done

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
        # What it did see, because "not found" on its own is not a diagnosis
        # and this console is all anyone gets on a machine that will not boot.
        echo "  /sys/class/net: $(ls /sys/class/net 2>/dev/null | tr '\n' ' ')"
        echo "  net drivers loaded: $(lsmod 2>/dev/null | grep -cE '^(virtio_net|e1000|e1000e|igb|ixgbe|bnx2|tg3|r8169|mlx)')"
        echo "  network PCI devices:"
        for d in /sys/bus/pci/devices/*/; do
            cls=$(cat "$d/class" 2>/dev/null)
            case "$cls" in 0x02*) echo "    $(basename "$d") $(cat "$d/modalias" 2>/dev/null)" ;; esac
        done
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
    # DHCP.
    #
    # udhcpc does not configure anything itself: it obtains a lease and hands
    # the values to a script, and everything an interface needs happens there.
    # This was `-s /bin/true`, so every boot took a lease from the server —
    # visible in the server's lease table, which made it look like it had
    # worked — and applied none of it. The node came up with an address
    # allocated to it and no address on it.
    if ! udhcpc -i "$IFACE" -s /usr/share/udhcpc/default.script -q -n -t 10; then
        echo "WARNING: DHCP failed, trying link-local..."
        ip addr add 169.254.1.1/16 dev "$IFACE"
    fi
fi

NETADDR="$(ip addr show "$IFACE" | grep 'inet ' | awk '{print $2}')"
if [ -n "$NETADDR" ]; then
    echo "Network: $IFACE $NETADDR $(ip route show default | head -1)"
    stamp "network up"

    # A name, so this node is distinguishable from the next one.
    #
    # Without it the kernel's hostname is "(none)", and every node on the
    # multicast stream says so — which is fine for one node and useless for
    # the second. DHCP's own name wins when it offers one, because a site that
    # names its machines has already decided; otherwise the MAC, which is the
    # one identifier a machine has before anyone has configured anything.
    NODE_NAME="$(cat /run/dhcp-hostname 2>/dev/null || true)"
    if [ -z "$NODE_NAME" ]; then
        MAC="$(cat "/sys/class/net/$IFACE/address" 2>/dev/null | tr -d ':')"
        [ -n "$MAC" ] && NODE_NAME="storm-$(echo "$MAC" | tail -c 7)"
    fi
    if [ -n "$NODE_NAME" ]; then
        echo "$NODE_NAME" > /proc/sys/kernel/hostname 2>/dev/null || true
        echo "  hostname: $NODE_NAME"
    fi
else
    # An empty summary is the symptom that hid a broken DHCP script for as
    # long as it did; say what it means instead of printing a blank.
    echo "WARNING: $IFACE has no address — nothing on this node will be reachable"
fi

# The clock, before anything timestamps anything.
#
# A node boots with whatever the RTC says, and a virtual machine or a board
# without a battery starts near the epoch. Every log line, every syslog frame
# and every file this node writes carries that time, and a viewer that orders
# by timestamp puts a whole boot in 1970 — or worse, plausibly wrong. Setting
# it here, before the root filesystem is even mounted, means the node's own
# account of its boot is orderable against everything else.
#
# One shot, from the lease's NTP servers or whatever the command line named.
# Bounded and non-fatal: a node with no time source still boots, it just says
# so, and the syslog frames carry a nil timestamp rather than a lie.
NTP_SERVERS="$(cat /run/ntp-servers 2>/dev/null || true)"
for t in $(cat /proc/cmdline); do
    case "$t" in rd.stormblock.ntp=*) NTP_SERVERS="${t#rd.stormblock.ntp=}" ;; esac
done
# A fallback, so a node without a DHCP option 42 still knows what time it is.
#
# Anycast addresses as well as a name: a node whose DHCP offered no NTP server
# may well have offered no usable DNS either, and a clock that depends on
# resolution is a clock that fails in exactly the situation it is needed. The
# names are tried first because a site that runs its own pool should win.
# How long the boot is willing to spend on this, total.
NTP_TIMEOUT=5
if [ -z "$NTP_SERVERS" ]; then
    # The public fallback is deliberately *not* used here: it is a WAN round
    # trip on a path that has not been proven, and this is the one place where
    # waiting delays everything behind it. It belongs after the root is up,
    # where a retry costs nothing and can keep trying.
    # Addresses, not names. busybox ntpd sets the clock from these in about a
    # second — measured — but given a *name* it waits on a resolver that has
    # existed for a fraction of a second, and that wait was 50 seconds of a
    # 74-second boot. There is no DNS to depend on here and no reason to: an
    # anycast address for a public time service is as stable as its name.
    #
    #   time.cloudflare.com  162.159.200.1     time.google.com  216.239.35.0
    #   pool.ntp.org and 1.amazon.pool.ntp.org rotate, so they are names only
    #   and belong to the after-boot retry, not here.
    NTP_SERVERS="162.159.200.1,216.239.35.0"
    NTP_FALLBACK=yes
    NTP_TIMEOUT=3
fi

if [ -n "$NTP_SERVERS" ]; then
    NTP_ARGS=""
    for srv in $(echo "$NTP_SERVERS" | tr ',' ' '); do
        NTP_ARGS="$NTP_ARGS -p $srv"
    done
    # -q: set the clock once and exit. -n: stay in the foreground so the wait
    # is this script's, not a stray daemon's.
    # Bounded, hard.
    #
    # An unreachable NTP server costs whatever the client is willing to wait,
    # and busybox ntpd is willing to wait a long time: the public fallback took
    # **50 seconds** of a 74-second boot before giving up. Nothing before the
    # root filesystem needs the clock except the timestamps on these very
    # lines, and a boot that is a minute slower to fix them has made a poor
    # trade.
    #
    # So: a few seconds against whatever DHCP offered, which on a working
    # network answers in milliseconds, and no more. A node that could not set
    # its clock here says so and carries on; the fix for that is a retry after
    # the root is up, where waiting costs nothing.
    if timeout "$NTP_TIMEOUT" ntpd -n -q -N $NTP_ARGS >/dev/null 2>&1; then
        echo "Clock:   $(date -u '+%Y-%m-%dT%H:%M:%SZ') (ntp: $NTP_SERVERS${NTP_FALLBACK:+, fallback})"
        stamp "clock set"
    else
        echo "WARNING: could not set the clock from $NTP_SERVERS — timestamps will be wrong"
        stamp "clock not set"
    fi
else
    echo "WARNING: no NTP server offered or configured — timestamps will be wrong"
fi
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

stamp "root ready"
echo "Switching to real root..."

# The kernel's own drivers and firmware, from the golden that carries them.
#
# The initramfs holds only what is needed to *reach* the root. Everything else
# — the full module tree and the firmware for anything this machine turns out
# to have — is a golden in the kernel pallet, mounted by the command line at
# /usr/lib/kernel. Binding it over /lib/modules and /lib/firmware is what makes
# the root filesystem find it: the kernel's firmware loader and modprobe both
# look there, and neither knows or cares that it came from a separate volume.
#
# Best effort. A node whose modules golden did not mount still has whatever the
# initramfs brought, which is enough to be running and to be fixed.
if [ -d /sysroot/usr/lib/kernel/lib/modules ]; then
    mkdir -p /sysroot/lib/modules /sysroot/lib/firmware
    mount --bind /sysroot/usr/lib/kernel/lib/modules /sysroot/lib/modules 2>/dev/null \
        && echo "  modules: /usr/lib/kernel -> /lib/modules"
    if [ -d /sysroot/usr/lib/kernel/lib/firmware ]; then
        mount --bind /sysroot/usr/lib/kernel/lib/firmware /sysroot/lib/firmware 2>/dev/null \
            && echo "  firmware: /usr/lib/kernel -> /lib/firmware"
    fi
else
    echo "  WARNING: no modules golden at /usr/lib/kernel — this node has only the"
    echo "           drivers the initramfs carried"
fi

# What the initramfs learned about the network, handed to the root that will
# use it. Nothing after switch_root runs a DHCP client, so a node whose
# resolver was configured here and not carried over resolves nothing.
if [ -n "$NODE_NAME" ] && [ -d /sysroot/etc ]; then
    echo "$NODE_NAME" > /sysroot/etc/hostname 2>/dev/null || true
fi
if [ -s /etc/resolv.conf ] && [ -d /sysroot/etc ]; then
    cp /etc/resolv.conf /sysroot/etc/resolv.conf 2>/dev/null || true
fi

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
echo "         [rd.stormblock.mount=<vol>:<path>,...]  — export and MOUNT these into the real root"
echo "                 for a PID 1 that is not systemd (stormpump reads directories, not fstab)"
