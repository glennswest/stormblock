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

# Firmware — only what is needed to reach the root.
#
# An HBA that loads firmware at probe cannot find the disk without it, and the
# disk is where every other piece of firmware lives. That is the whole of what
# has to be here: 7.7 MB of Fibre Channel and converged-adapter blobs.
#
# Everything else went to the kernel pallet's `modules` golden — 40 MB of NIC,
# Bluetooth, SoC and vendor firmware that the root filesystem mounts moments
# later. A NIC whose firmware moved cannot come up before the root does, so
# `/init` brings the network up again after the golden is bound, for the
# machines where that matters. A disk that cannot be reached has no such second
# chance, which is why these stay.
FWDIR="$(ls -d /usr/lib/firmware /lib/firmware 2>/dev/null | head -1 || true)"
# Named by adapter family. Storage adapters are a small and slow-moving set —
# unlike NIC model numbers, which is why the split is drawn here.
FW_STORAGE="ql2*_fw.bin* qla*.bin* lpfc* aic94xx* mpt* qed* bnx2 bnx2x cxgb4
            phanfw.bin* vxge qlogic emulex advansys"
if [ -n "$FWDIR" ] && [ -d "$FWDIR" ]; then
    mkdir -p "$INITRD_DIR/lib/firmware"
    for pat in $FW_STORAGE; do
        for e in "$FWDIR"/$pat; do
            [ -e "$e" ] && cp -a "$e" "$INITRD_DIR/lib/firmware/" 2>/dev/null || true
        done
    done
    echo "  firmware:   $(du -sh "$INITRD_DIR/lib/firmware" | cut -f1) — storage adapters only (of $(du -sh "$FWDIR" | cut -f1); the rest is in the modules golden)"
else
    echo "  WARNING: no linux-firmware on this build host — an HBA that loads"
    echo "           firmware at probe will look like a missing driver"
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
# Where this kernel's modules live.
#
# `MODROOT` so an image can be built for a kernel the build host is not running.
# Without it the image inherits whatever the box happens to have booted, which
# makes the kernel in a release an accident of scheduling rather than a choice —
# and means two builds of the same commit can ship different kernels.
MODROOT="${MODROOT:-}"
MODDIR="$MODROOT/lib/modules/$KVER"
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
    BEFORE=$(lsmod 2>/dev/null | tail -n +2 | wc -l)
    NOW_BEFORE="$BEFORE"
    while [ $PASS -lt 5 ]; do
        PASS=$((PASS + 1))
        for ma in /sys/bus/*/devices/*/modalias; do
            [ -f "$ma" ] || continue
            modprobe -q "$(cat "$ma")" 2>/dev/null || true
        done
        AFTER=$(lsmod 2>/dev/null | tail -n +2 | wc -l)
        LOADED=$((AFTER - BEFORE))
        # Stop when a pass loads nothing new.
        #
        # The previous test counted modprobe *successes*, and modprobe succeeds
        # for a module that is already loaded — so every pass "found" something
        # and the loop always ran its full five, sleeping a second between each.
        # Four seconds of a thirteen-second initramfs, spent asking for drivers
        # that were already there. The count of loaded modules is the honest
        # measure of whether another pass is worth taking.
        [ "$AFTER" -eq "$NOW_BEFORE" ] && break
        NOW_BEFORE="$AFTER"
        [ $PASS -ge 5 ] && break
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
# `-x` is not enough: udevadm is dynamically linked, and a copy made without
# its libraries is executable and still fails as "udevadm: not found". That
# happened here — udevd started, every udevadm call failed, and nothing could
# trigger, settle or *stop* it. A udevd that cannot be controlled is worse than
# no udevd, so this asks it to run before relying on it.
if [ -x /usr/lib/systemd/systemd-udevd ] && udevadm --version >/dev/null 2>&1; then
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

# Bring the node up **on a bridge**, the way every hypervisor does.
#
# A VM's NIC is a tap, and a tap has to hang off something. Attaching one
# straight to the interface that carries the node's own address is not
# possible — a tap is not a port of a physical NIC — so without a bridge the
# only options are NAT (a private network the LAN cannot reach) or macvtap
# (which deliberately stops the node talking to its own guests). Both are
# worse than moving the node's address onto a bridge that its uplink is a port
# of, which is what Proxmox, libvirt and every other hypervisor does.
#
# **With a fallback, because this is the one step that can strand a node.** If
# any part of it fails the interface is left exactly as it was and the boot
# carries on with plain DHCP on the uplink — a node with no VM networking is a
# node; a node with no networking is a recovery job.
BRIDGE="${STORM_BRIDGE:-stormbr0}"
UPLINK="$IFACE"
if [ -z "${NO_BRIDGE:-}" ] && ip link add name "$BRIDGE" type bridge 2>/dev/null; then
    if ip link set "$IFACE" master "$BRIDGE" 2>/dev/null && ip link set "$BRIDGE" up; then
        # Everything below configures the bridge instead: it is the interface
        # that now holds the address, and the uplink is one of its ports.
        echo "  bridged: $IFACE is a port of $BRIDGE"
        IFACE="$BRIDGE"
    else
        echo "WARNING: could not enslave $IFACE to $BRIDGE — no VM networking"
        ip link del "$BRIDGE" 2>/dev/null || true
    fi
fi

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
        # The *uplink's* MAC, not the bridge's: a bridge takes a random
        # address until it has a port, so naming a node after it would give
        # the same machine a different name every boot.
        MAC="$(cat "/sys/class/net/${UPLINK:-$IFACE}/address" 2>/dev/null | tr -d ':')"
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

# The clock is not set here.
#
# It used to be, and it cost 50 seconds when the name it was given would not
# resolve, then 5 seconds once bounded — for a machine whose RTC is already
# close enough that nothing in this script would read a different value.
# Meanwhile the only consumer of these timestamps is the console text, which
# carries kernel-relative times anyway.
#
# So it moved to where it belongs: `timesync` is a supervised service on the
# node, started immediately after the root is up, and it keeps the clock rather
# than setting it once. Nothing waits on it, and a time server that is slow or
# gone delays nothing.
#
# What the lease offered is written down for that service to use:
#   /run/ntp-servers   DHCP option 42, if any

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

# A second chance at the network, now that all the firmware is here.
#
# The initramfs carries only the firmware that reaches the *root*; a NIC whose
# blob lives in the modules golden could not come up before the golden was
# mounted, and it has just been mounted. Binding it over the initramfs's own
# /lib/firmware as well is what lets the kernel find it, and then one more
# discovery pass and one more DHCP attempt is all it takes.
#
# Only when there is no address: on the overwhelming majority of machines the
# network came up long ago and this does nothing.
if [ -z "$(ip -4 addr show scope global 2>/dev/null | grep -m1 'inet ')" ] \
   && [ -d /sysroot/usr/lib/kernel/lib/firmware ]; then
    echo "No address yet — retrying with the full firmware set"
    mount --bind /sysroot/usr/lib/kernel/lib/firmware /lib/firmware 2>/dev/null || true
    for ma in /sys/bus/*/devices/*/modalias; do
        [ -f "$ma" ] || continue
        modprobe -q "$(cat "$ma")" 2>/dev/null || true
    done
    for dev in /sys/class/net/*; do
        n=$(basename "$dev"); [ "$n" = "lo" ] && continue
        ip link set "$n" up 2>/dev/null
        udhcpc -i "$n" -s /usr/share/udhcpc/default.script -q -n -t 5 >/dev/null 2>&1 && break
    done
    stamp "network retried: $(ip -4 addr show scope global 2>/dev/null | grep -m1 'inet ' | awk '{print $2}')"
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

# Stop udev before handing over.
#
# **A udevd started here keeps running across switch_root**, against a root
# that is about to disappear, with rules and helpers that are about to go with
# it. It sits harmless for exactly as long as nothing generates a uevent — and
# then the first kernel module loaded on the real root produced 14,000
# udev-workers and OOM-killed the node at 16 seconds. The failure looks like
# whatever ran last, not like the initramfs that left this behind.
#
# Every distribution's initramfs stops udev before switch_root. This one did
# not, and could not: udevadm was unusable, so even `udevadm control --exit`
# was unavailable. Both doors are tried, because the point is that udevd does
# not survive this line.
if udevadm --version >/dev/null 2>&1; then
    udevadm control --exit 2>/dev/null || true
fi
for _p in $(pidof systemd-udevd udevd 2>/dev/null); do
    kill "$_p" 2>/dev/null || true
done

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
