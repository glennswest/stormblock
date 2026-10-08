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
# Absolute: the main archive is appended after a `cd` into the staging tree,
# so a relative path would put it there, and the image would be microcode alone.
case "$OUTPUT" in /*) ;; *) OUTPUT="$PWD/$OUTPUT" ;; esac

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
# Where a boot hook goes (#109). Created empty and always present, so an image
# that carries no hook and one that does differ by a file rather than by a
# path: /init tests for executables in here and does nothing when there are
# none.
mkdir -p "$INITRD_DIR/etc/stormblock/boot.d"
# When this image was built, in epoch seconds: the floor /init sets the clock
# to when no time server answers and the clock reads earlier (#251). A
# reproducible build says when with SOURCE_DATE_EPOCH.
BUILD_EPOCH="${SOURCE_DATE_EPOCH:-$(date -u +%s)}"
echo "$BUILD_EPOCH" > "$INITRD_DIR/etc/stormblock/build-date"

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

# What a whole subtree brings that this initramfs cannot use.
#
# `kernel/drivers/net` is taken whole on purpose — a node has to DHCP on
# whatever card it has, and a list of model numbers is a list that goes stale.
# But "whole" is a promise about *the tree it is given*, and that tree grew: an
# earlier build had only `kernel-modules-core`, and adding `kernel-modules`
# put every wireless driver in Fedora under `drivers/net` as well. They came
# in, and their stacks came with them through the dependency closure below —
# 802.11, Bluetooth for the combo chips, SDIO for the ones on an MMC bus. The
# archive went from 65.7 MB to 97.5 MB, and every byte of it is read into RAM
# on every boot of every node.
#
# None of it can be a boot path. This initramfs has two jobs: reach the root
# disk, and get a lease on the wire. A node does not netboot over Wi-Fi, or
# over cellular, or over CAN. Whole *classes*, not model numbers, so nothing
# here goes stale and the promise above still holds for every card that could
# actually carry a boot.
#
# Pruned before the closure runs, deliberately: anything genuinely depended on
# by a driver that stays is copied back by the closure, so this can remove too
# much but cannot remove something needed.
PRUNE_CLASSES="wireless wwan can ieee802154 wan hamradio"
pruned_bytes=0
pruned_dirs=""
for class in $PRUNE_CLASSES; do
    d="$DEST/kernel/drivers/net/$class"
    [ -d "$d" ] || continue
    pruned_bytes=$((pruned_bytes + $(du -sk "$d" | cut -f1)))
    pruned_dirs="$pruned_dirs $class"
    rm -rf "$d"
done
if [ -n "$pruned_dirs" ]; then
    echo "  modules:    dropped$pruned_dirs — $((pruned_bytes / 1024)) MB that cannot carry a boot"
fi

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
        # Either option: 15 is a single domain name, 119 is a search list, and
        # a server may send one, the other, or both. This network sends 119
        # only, so reading `$domain` alone found nothing — and the node then
        # asked for `boothost` unqualified, which resolves nowhere.
        dhcp_domain="$domain"
        [ -z "$dhcp_domain" ] && dhcp_domain="${search%% *}"
        if [ -n "$dhcp_domain" ]; then
            echo "search $dhcp_domain" >> /etc/resolv.conf
            # Written down as well as configured. What the resolver was told
            # and what this domain *is* are two questions, and the second one
            # should not be answered by parsing the answer to the first.
            echo "$dhcp_domain" > /run/dhcp-domain
        fi
        for d in $dns; do
            echo "nameserver $d" >> /etc/resolv.conf
        done
        # What the lease actually offered, written down so the next thing that
        # needs it does not have to infer it from a file it may not have got.
        [ -n "$dns" ] && echo "$dns" > /run/dhcp-dns
        # The lease usually carries NTP servers (DHCP option 42). Written down
        # rather than used here, because the clock is set once, after the
        # network is up, and not on every renewal.
        [ -n "$ntpsrv" ] && echo "$ntpsrv" > /run/ntp-servers
        # DHCP option 12, if the server has an opinion about what this machine
        # is called. It usually knows better than the machine does.
        [ -n "$hostname" ] && echo "$hostname" > /run/dhcp-hostname
        # Where this network keeps its images: DHCP option 17, root-path.
        #
        # A diskless node has to reach an appliance, and the address of one is
        # the single most network-specific fact there is - so it cannot be in
        # the image, and it is not this node's to guess. The network already
        # answers questions like this: the same lease says which resolver and
        # which clock to use. Only a URL is taken, because option 17 is
        # classically an NFS export and a path is not an appliance.
        case "$rootpath" in
            http://*|https://*|nvme-tcp://*) echo "$rootpath" > /run/stormblock-boothost ;;
        esac
        # Who answered, and who they named as the boot server. Written down
        # rather than acted on: on a network that says nothing else, one of
        # these two usually is the appliance, and on a network that does say
        # something they are simply not needed. Which is which is settled by
        # asking them, not by assuming.
        [ -n "$serverid" ] && echo "$serverid" > /run/dhcp-serverid
        [ -n "$siaddr" ] && echo "$siaddr" > /run/dhcp-siaddr
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

# Boot hooks — who decides where this node boots from (#109).
#
# `BOOT_HOOKS="/path/to/zeroboot /path/to/50-something"` installs them into
# /etc/stormblock/boot.d, where /init runs them in name order before it probes
# the device the command line names. The installer that owns a hook may also
# drop it in itself; this is here so that a build that knows about one does not
# have to unpack and repack the image to add it.
#
# **A dynamically linked hook is refused.** There is no loader in this
# initramfs, so a glibc build fails at boot as "not found" — on a file that is
# plainly there, with the executable bit set, which is as misleading as an
# error gets. Static musl, or a shell script.
for hook_src in ${BOOT_HOOKS:-}; do
    if [ ! -f "$hook_src" ]; then
        echo "ERROR: boot hook not found: $hook_src"
        exit 1
    fi
    if head -c 4 "$hook_src" | LC_ALL=C grep -aq ELF; then
        if LC_ALL=C grep -aq -e '/ld-linux' -e '/ld-musl' "$hook_src"; then
            echo "ERROR: boot hook $hook_src is dynamically linked."
            echo "  There is no loader in this initramfs: it would fail at boot as"
            echo "  'not found' on a file that is plainly there. Build it static."
            exit 1
        fi
    elif head -c 2 "$hook_src" | grep -q '#!'; then
        hook_interp=$(head -c 128 "$hook_src" | sed -n '1s|^#! *\([^ ]*\).*|\1|p')
        case "$hook_interp" in
            /bin/sh|/bin/ash|/bin/busybox) ;;
            *) echo "  WARNING: boot hook $(basename "$hook_src") wants $hook_interp, which this image may not have" ;;
        esac
    fi
    install -m 755 "$hook_src" "$INITRD_DIR/etc/stormblock/boot.d/$(basename "$hook_src")"
    echo "  boot hook:  $(basename "$hook_src") ($(du -h "$hook_src" | cut -f1))"
done

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

# Every console= hears the boot, not only the last one (#237).
#
# The kernel prints to every console= on its command line, but /dev/console —
# which is all this script and everything it starts write to — is the *last*
# one. With `console=tty0 console=ttyS0,115200n8` a server's screen shows the
# kernel's lines and then nothing: the install, the flow-over and a FATAL all
# go to serial alone, and on a blade whose serial-over-LAN drops at every
# reset that is nowhere anyone is looking.
#
# So when two or more consoles are there, stdout and stderr go through a fifo
# to a tee onto each of them. The order on the command line is left alone:
# serial stays what /dev/console is, for the VMs and the console capture that
# read it.
#
# The fan-out must not be a pipe that can break. The engine started below
# writes to it and serves the root device; its println! panics on EPIPE. So
# the reader ignores every signal it can, holds the fifo's read end itself,
# and runs tee again if tee is killed. It ends when the last writer closes.
# --- BEGIN console fan-out (covered by tests/initramfs-console.sh)
console_list() { # cmdline -> the console devices that exist and open, in order
    _seen=" "
    for _p in $1; do
        case "$_p" in console=*) ;; *) continue ;; esac
        _n=${_p#console=}; _n=${_n%%,*}; _n=${_n#/dev/}
        case "$_n" in ""|*/*) continue ;; esac
        _d="${STORM_CONSOLE_DEV:-/dev}/$_n"
        case "$_seen" in *" $_d "*) continue ;; esac
        [ -e "$_d" ] || continue
        # A serial port with no UART behind it: the node is there and opens,
        # and every write is EIO. The 8250 driver says so as type 0.
        [ "$(cat "${STORM_CONSOLE_SYS:-/sys/class/tty}/$_n/type" 2>/dev/null)" = 0 ] && continue
        ( : >> "$_d" ) 2>/dev/null || continue
        _seen="$_seen$_d "
        printf '%s\n' "$_d"
    done
}

console_fanout() { # devices... — stdout and stderr go to each of them
    [ $# -ge 2 ] || return 0
    command -v mkfifo >/dev/null 2>&1 || return 0
    _fifo="${STORM_CONSOLE_FIFO:-/run/console.fifo}"
    rm -f "$_fifo"
    mkfifo "$_fifo" 2>/dev/null || return 0
    (
        trap '' HUP INT QUIT TERM PIPE
        exec 3<"$_fifo" </dev/null >/dev/null 2>&1
        # tee exits 0 at the end of input; anything else (killed, or a
        # console that failed a write) is run again, and at the end of input
        # the rerun reads nothing and exits 0.
        until tee -a "$@" <&3 >/dev/null; do :; done
    ) &
    CONSOLE_FANOUT_PID=$!
    exec >"$_fifo" 2>&1
}

console_restore() { # back to /dev/console alone: for switch_root, for a shell
    [ -n "$CONSOLE_FANOUT_PID" ] || return 0
    exec >/dev/console 2>&1
}

# The emergency shell, on every console: /bin/sh on /dev/console as always,
# and a shell of its own on each other console, so whoever is at the screen
# gets a prompt as well as whoever is on serial.
#
# Which console /dev/console is, the kernel says: the last entry of
# /sys/class/tty/console/active. Not "the last console=": two serial ports
# share the 8250 driver's one console, so console=ttyS1 console=ttyS0 makes
# ttyS1 /dev/console (seen in ci-console-verify.sh). A VT is one console
# whatever its number (tty0 is the VT in front), so a VT /dev/console gets no
# second shell on another tty<N>: two shells reading one keyboard.
console_primary() {
    _last=""
    for _n in $(cat "${STORM_CONSOLE_ACTIVE:-/sys/class/tty/console/active}" 2>/dev/null); do
        _last="${STORM_CONSOLE_DEV:-/dev}/$_n"
    done
    [ -n "$_last" ] || for _d in $CONSOLES; do _last="$_d"; done
    printf '%s\n' "$_last"
}

rescue_shell() {
    echo "Dropping to shell..."
    console_restore
    _primary=$(console_primary)
    if command -v setsid >/dev/null 2>&1; then
        for _d in $CONSOLES; do
            [ "$_d" = "$_primary" ] && continue
            case "${_primary##*/} ${_d##*/}" in
                tty[0-9]*" "tty[0-9]*) continue ;;
            esac
            setsid sh -c 'exec sh -i <"$1" >"$1" 2>&1' sh "$_d" &
        done
    fi
    exec /bin/sh
}

CONSOLE_FANOUT_PID=""
CONSOLES=$(console_list "$(cat "${STORM_CMDLINE:-/proc/cmdline}" 2>/dev/null)" | tr '\n' ' ')
# shellcheck disable=SC2086
console_fanout $CONSOLES
# --- END console fan-out

# Parse kernel cmdline parameters
PORTAL=""
IQN=""
LAYOUT=""
PORT="3260"
IP_CONF=""
SLAB=""
BOOTHOST=""
ENGINE_LOG="${STORM_ENGINE_LOG:-/run/stormblock/engine.log}"
BOOTTAG=""
BOOTTAG_FROM=""
TRUST_SMBIOS=""
HOSTNQN=""
META=""
VOLUME=""
OVERLAY=""
IMAGE_STORE=""
WRITABLE=""
MOUNTS=""
NTP_MODE=""
DECLARED_NET=""

for param in $(cat /proc/cmdline); do
    case "$param" in
        rd.stormblock.portal=*)      PORTAL="${param#*=}" ;;
        rd.stormblock.iqn=*)         IQN="${param#*=}" ;;
        rd.stormblock.layout=*)      LAYOUT="${param#*=}" ;;
        rd.stormblock.port=*)        PORT="${param#*=}" ;;
        rd.stormblock.slab=*)        SLAB="${param#*=}" ;;
        # Diskless: ask the appliance which image this machine boots. The
        # cmdline is baked into the image and identical on every node that
        # boots it, so it names the appliance, never the namespace.
        rd.stormblock.boothost=*)    BOOTHOST="${param#*=}" ;;
        rd.stormblock.bootport=*)    BOOTPORT="${param#*=}" ;;
        rd.stormblock.assimilate=*)  ASSIMILATE="${param#*=}" ;;
        rd.stormblock.wipe=*)        WIPE="${param#*=}" ;;
        # A guessed (SMBIOS) identity may install over a disk after all: for
        # an image whose machines are named by serial and booted without
        # stormbootx (#249).
        rd.stormblock.trust-smbios=*) TRUST_SMBIOS="${param#*=}" ;;
        # A drive behind a SAS expander or in an SES enclosure is never taken
        # by the survey (#273): a disk shelf. `1` for a server whose own bays
        # sit behind one.
        rd.stormblock.allow-external=*) ALLOW_EXTERNAL="${param#*=}" ;;
        rd.stormblock.bond=*)        BOND_MODE="${param#*=}" ;;
        # `off`: no NTP step in the initramfs (#251); the build-date floor
        # still applies.
        rd.stormblock.ntp=*)         NTP_MODE="${param#*=}" ;;
        # `off`: the boot NIC ignores the node's declared [network] (#229).
        rd.stormblock.declared-net=*) DECLARED_NET="${param#*=}" ;;
        rd.stormblock.tag=*)         BOOTTAG="${param#*=}" ;;
        # What to call ourselves on every NVMe connect. stormbootx composed
        # this from SMBIOS and presented it to load the kernel; presenting the
        # same name here means the appliance sees one machine across the
        # handover instead of a nameless second initiator.
        rd.stormblock.hostnqn=*)     HOSTNQN="${param#*=}" ;;
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

# The drive the command line names, before any probe replaces SLAB with a
# claimed image: the one local drive an install may take (#273).
SLAB_NAMED="$SLAB"

# --- BEGIN boot identity (covered by tests/initramfs-boot-hook.sh)
# Who this machine is, as the firmware that loaded this kernel claimed it
# (#249).
#
# stormbootx names the machine before Linux exists - by DHCP and reverse DNS
# on chassis that share a serial, by the engine's own name for it when the
# claim reply gives one - and claims `boothost/<name>`. Working the name out
# again here, from SMBIOS, gets a different answer exactly where it matters:
# the eight blades of a Supermicro MicroCloud all report the chassis serial,
# and server8 claimed server1's old synonym, booted 11.58 from the firmware
# and then laid its disk from 11.50.
#
# So the firmware hands the name down, in two volatile EFI variables it sets
# before it starts the loader (BOOTSERVICE_ACCESS | RUNTIME_ACCESS, not
# non-volatile: they die with the boot that set them, and a stale one can
# never name the next boot):
#
#   StormBootTag-<STORMBOOT_GUID>      the name it claimed boothost/<name> on
#   StormBootHostNqn-<STORMBOOT_GUID>  the host NQN it attached as
#
# efivarfs shows each as four attribute bytes and then the value. Nothing
# about the pallet's command line or the loader between the two changes.
#
# Order: the firmware's variable, then `rd.stormblock.tag=`, then (in
# boothost_claim) the SMBIOS serial and UUID. Where it came from is kept in
# BOOTTAG_FROM, because a guess is not allowed to destroy a disk (below).
STORMBOOT_GUID="ab361f54-0166-44a4-a088-1ac22e98ab76"
EFIVARS="${STORM_EFIVARS:-/sys/firmware/efi/efivars}"
if [ -z "${STORM_EFIVARS:-}" ] && [ -d /sys/firmware/efi ] \
   && [ -z "$(ls -A "$EFIVARS" 2>/dev/null)" ]; then
    modprobe -q efivarfs 2>/dev/null || true
    mount -t efivarfs efivarfs "$EFIVARS" 2>/dev/null || true
fi
efi_value() { # name -> the variable's value, if it is set and plausible
    _f="$EFIVARS/$1-$STORMBOOT_GUID"
    [ -r "$_f" ] || return 0
    _v=$(tail -c +5 "$_f" 2>/dev/null | tr -d '\000\n\r ')
    # A name goes into a URL path, an NQN and a volume name: anything outside
    # this set is not one stormbootx would set, and is not used.
    case "$_v" in
    "") ;;
    *[!A-Za-z0-9._:-]*) echo "  ignoring $1: '$_v' is not a name" >&2 ;;
    *) printf '%s\n' "$_v" ;;
    esac
}
FW_TAG=$(efi_value StormBootTag)
FW_NQN=$(efi_value StormBootHostNqn)
if [ -n "$FW_TAG" ]; then
    if [ -n "$BOOTTAG" ] && [ "$BOOTTAG" != "$FW_TAG" ]; then
        echo "WARNING: rd.stormblock.tag=$BOOTTAG, but the firmware claimed as $FW_TAG - using $FW_TAG"
    fi
    BOOTTAG="$FW_TAG"
    BOOTTAG_FROM=firmware
    echo "Machine name from the firmware: $BOOTTAG"
elif [ -n "$BOOTTAG" ]; then
    BOOTTAG_FROM=cmdline
fi
# The engine claims again to finish a cut-short flow-over (#171): it must
# claim as this machine, not work the name out again from SMBIOS (#259).
[ -n "$BOOTTAG" ] && export STORMBLOCK_BOOT_TAG="$BOOTTAG"
if [ -n "$FW_NQN" ]; then
    if [ -n "$HOSTNQN" ] && [ "$HOSTNQN" != "$FW_NQN" ]; then
        echo "WARNING: rd.stormblock.hostnqn=$HOSTNQN, but the firmware attached as $FW_NQN - using $FW_NQN"
    fi
    HOSTNQN="$FW_NQN"
fi
# Whether this boot's identity is a guess: SMBIOS read here, not a name the
# firmware or the operator gave. A guess may claim an image to boot - there
# is nothing better to boot - but never installs over a disk that carries a
# slab: a wrong guess there is another machine's release laid over this one's.
identity_guessed() {
    [ "$BOOTTAG_FROM" = smbios ] && [ "${TRUST_SMBIOS:-}" != 1 ]
}
# --- END boot identity

# --- BEGIN install config read (covered by tests/initramfs-install-config.sh)
# The node's install-config.yaml, from the boot media (#275, stormbootx#79,
# stormcos#82). storminstall writes it on the ISO's ESP; stormbootx hands it
# down in volatile EFI variables under STORMBOOT_GUID:
#
#   StormBootInstallConfig          v1:<length>:<chunks>:<sha256, hex>
#   StormBootInstallConfig0..N-1    the bytes, 768 per chunk
#
# The header is set last, so no header means nothing was handed down. The
# file is used only when its length and digest match the header, staged in
# RAM (0600) until /state is mounted (below), and every variable is deleted
# now whatever came of it: efivarfs files are world-readable, and this one
# carries pullSecret and apiToken. Its content is never printed.
INSTALL_CONFIG_STAGED=""
install_config_forget() { # delete every StormBootInstallConfig variable
    for _icf in "$EFIVARS"/StormBootInstallConfig*-"$STORMBOOT_GUID"; do
        [ -e "$_icf" ] || continue
        chattr -i "$_icf" 2>/dev/null
        rm -f "$_icf" 2>/dev/null
    done
}
install_config_read() {
    _ich=$(efi_value StormBootInstallConfig 2>/dev/null)
    if [ -z "$_ich" ]; then
        # Set but not a header this reads (efi_value refused it): its chunks
        # still carry secrets.
        if [ -e "$EFIVARS/StormBootInstallConfig-$STORMBOOT_GUID" ]; then
            echo "install-config: the firmware's header is not one this initramfs reads - ignored"
            install_config_forget
        fi
        return 0
    fi
    _icv=$(printf '%s' "$_ich" | cut -d: -f1)
    _icl=$(printf '%s' "$_ich" | cut -d: -f2)
    _icn=$(printf '%s' "$_ich" | cut -d: -f3)
    _ics=$(printf '%s' "$_ich" | cut -d: -f4)
    case "$_icl" in ''|*[!0-9]*) _icv=bad ;; esac
    case "$_icn" in ''|*[!0-9]*) _icv=bad ;; esac
    case "$_ics" in *[!0-9a-f]*) _icv=bad ;; esac
    if [ "$_icv" != v1 ] || [ "${#_ics}" -ne 64 ] || [ "$_icn" -lt 1 ] || [ "$_icn" -gt 1024 ] \
       || [ "$(printf '%s' "$_ich" | tr -cd ':' | wc -c)" -ne 3 ]; then
        echo "install-config: the firmware's header is not one this initramfs reads - ignored"
        install_config_forget
        return 0
    fi
    _ico="${STORM_RUN:-/run/stormblock}/install-config.yaml"
    mkdir -p "${_ico%/*}"
    rm -f "$_ico"
    ( umask 077; : > "$_ico" )
    _ici=0
    while [ "$_ici" -lt "$_icn" ]; do
        _icc="$EFIVARS/StormBootInstallConfig$_ici-$STORMBOOT_GUID"
        if [ ! -r "$_icc" ]; then
            echo "install-config: chunk $_ici of $_icn is missing - not used"
            rm -f "$_ico"
            install_config_forget
            return 0
        fi
        tail -c +5 "$_icc" >> "$_ico"
        _ici=$((_ici + 1))
    done
    _icg=$(wc -c < "$_ico" | tr -d ' ')
    _icd=$(sha256sum "$_ico" | cut -d' ' -f1)
    if [ "$_icg" != "$_icl" ] || [ "$_icd" != "$_ics" ]; then
        echo "install-config: $_icg byte(s) with sha256 $_icd, but the firmware said $_icl and $_ics - not used"
        rm -f "$_ico"
        install_config_forget
        return 0
    fi
    INSTALL_CONFIG_STAGED="$_ico"
    echo "install-config: $_icl bytes from the boot media, sha256 $_ics (verified)"
    install_config_forget
}
install_config_read
# --- END install config read

# --- BEGIN mount list (covered by tests/initramfs-mounts.sh)
# Which volumes this init mounts, and where (#262).
#
# This used to be the command line alone, `rd.stormblock.mount=<vol>:<path>,…`:
# one word of ~1.8 KB on a line x86 caps at 2048 bytes. Past the cap the EFI
# stub truncates and boots anyway, and 11.68 came up with nothing mounted at
# all (stormcos#236). So the list lives with the release, in its root volume:
#
#   /etc/stormblock/mounts      one `<vol>:<path>` per line, `#` comments
#
# read out of the slab with `stormblock slab cat` before anything is exported
# - nothing is attached to read it. `rd.stormblock.mount=` still works, and
# wins when it is there: an older image carries its list nowhere else.
MOUNTS_CMDLINE="$MOUNTS"
MOUNTS_FROM=""
mounts_from() { # slab -> MOUNTS (comma-separated) and MOUNTS_FROM
    MOUNTS="$MOUNTS_CMDLINE"
    if [ -n "$MOUNTS" ]; then
        MOUNTS_FROM="the command line"
        return 0
    fi
    MOUNTS_FROM=""
    [ -n "${1:-}" ] || return 0
    _ml_out="${STORM_RUN:-/run/stormblock}/mounts.list"
    mkdir -p "${_ml_out%/*}" 2>/dev/null
    rm -f "$_ml_out"
    if "${STORM_STORMBLOCK:-/usr/sbin/stormblock}" slab cat --slab "$1" \
            --volume "${VOLUME:-stormpump}" --out "$_ml_out" /etc/stormblock/mounts \
            >/dev/null 2>&1 && [ -r "$_ml_out" ]; then
        MOUNTS=$(sed -e 's/#.*//' -e 's/[[:space:]]//g' "$_ml_out" | grep -v '^$' | tr '\n' ',' | sed 's/,$//')
        MOUNTS_FROM="/etc/stormblock/mounts in ${VOLUME:-stormpump}"
    fi
    return 0
}

# Optional entries (#288, stormcos#208): `?<vol>:<path>`.
#
# One stormpump golden serves every flavor of a release (cilium, flowsdn), so
# its list names volumes a given release does not carry. A plain entry is
# required, as before: missing, the disk cannot boot this node (the probe) and
# the engine dies on "volume not found". A `?` entry is mounted when the slab
# has the volume and left out when it does not - before the ublk numbers are
# handed out, so the devices and the mount points stay in step.
#
# Which volumes the slab has comes from `slab volumes`, read once and only
# when the list has a `?` in it. A slab that cannot say (no metadata on it,
# unreadable) leaves its optional entries out too, and says so: a mount left
# out costs one service, a volume asked for and not there costs the boot.
slab_has_volume() { # listing name
    printf '%s\n' "$1" | grep -qE ": volume $2 | $2\$"
}
MOUNTS_SKIPPED=""
mounts_optional() { # slab; MOUNTS -> MOUNTS without `?`, MOUNTS_SKIPPED
    MOUNTS_SKIPPED=""
    case ",$MOUNTS" in *",?"*) ;; *) return 0 ;; esac
    _mo_vols=""
    _mo_why=""
    if [ -n "${1:-}" ]; then
        _mo_vols=$("${STORM_STORMBLOCK:-/usr/sbin/stormblock}" slab volumes "$1" 2>/dev/null)
    fi
    if ! printf '%s\n' "$_mo_vols" | grep -q ': volume '; then
        _mo_why="${1:-no slab} cannot list its volumes"
    fi
    _mo_keep=""
    OIFS_MO=$IFS; IFS=,
    for _mo_e in $MOUNTS; do
        IFS=$OIFS_MO
        case "$_mo_e" in
        \?*)
            _mo_e="${_mo_e#?}"
            _mo_n="${_mo_e%%:*}"
            if [ -z "$_mo_why" ] && slab_has_volume "$_mo_vols" "$_mo_n"; then
                _mo_keep="$_mo_keep${_mo_keep:+,}$_mo_e"
            else
                MOUNTS_SKIPPED="$MOUNTS_SKIPPED${MOUNTS_SKIPPED:+,}$_mo_e"
                if [ -n "$_mo_why" ]; then
                    echo "  optional, left out: $_mo_n (${_mo_e#*:}): $_mo_why"
                else
                    echo "  optional, not in this release: $_mo_n (${_mo_e#*:})"
                fi
            fi
            ;;
        *) _mo_keep="$_mo_keep${_mo_keep:+,}$_mo_e" ;;
        esac
        IFS=,
    done
    IFS=$OIFS_MO
    MOUNTS="$_mo_keep"
    return 0
}

# What release a disk holds, for a message (#294): its root volume's
# os-release, read with nothing attached. "unknown" when it does not say.
disk_release() { # slab
    _dr="${STORM_RUN:-/run}/stormblock-disk-os-release"
    if "${STORM_STORMBLOCK:-/usr/sbin/stormblock}" slab cat --slab "$1" \
           --volume "${VOLUME:-stormpump}" --out "$_dr" /etc/os-release >/dev/null 2>&1 \
       && [ -s "$_dr" ]; then
        _rel=$(sed -n 's/^PRETTY_NAME="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$_dr" | head -1)
        [ -n "$_rel" ] || _rel=$(sed -n 's/^VERSION_ID="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$_dr" | head -1)
        rm -f "$_dr"
        printf '%s\n' "${_rel:-unknown (its os-release names none)}"
    else
        echo "unknown (no /etc/os-release in its '${VOLUME:-stormpump}' volume)"
    fi
}
# --- END mount list

# Local-slab boot (stormcos) when a slab is named on the cmdline, or when the
# initramfs carries a boot.toml handoff and no iSCSI portal was given.
#
# iSCSI is the mode that has to be *asked for*, because it needs a portal, an
# IQN and a layout and none of those can be discovered. Everything else is a
# stormblock boot.
#
# This used to require a slab or an appliance named on the command line, and
# defaulted to iSCSI when neither was there. Both are discovered now — the
# slab from the local disk, the appliance from DHCP — and discovery runs
# *after* this point, so an image that names neither chose iSCSI, failed
# validation for want of a portal, and never reached the code that would have
# found what it needed.
BOOT_MODE="local"
if [ -n "$PORTAL" ] && [ -z "$SLAB" ] && [ -z "$BOOTHOST" ]; then
    BOOT_MODE="iscsi"
fi
if [ -n "$SLAB" ]; then
    BOOT_MODE="local"
elif [ -n "$BOOTHOST" ]; then
    # Same path as a local slab from here on: only *where the slab is* differs,
    # and stormblock opens a fabric URI wherever it opens a device path. The
    # claim itself has to wait for the network, so it happens after that is up.
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
    rescue_shell
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
        echo "           symbols - a driver is present but its dependency is not:"
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

# --- BEGIN protocol halves (covered by tests/initramfs-netdev.sh)
# The network half of a driver that comes in two (#250).
#
# The walk above loads what a device's modalias names, and for most NICs that
# is the whole driver. Not for the ConnectX-3: its PCI ID (15b3:1003) names
# `mlx4_core`, and the Ethernet ports are `mlx4_en`, which matches only
# `auxiliary:mlx4_core.eth` - a device mlx4_core creates at the *end* of a
# probe that talks to the card's firmware for seconds. By then the walk has
# had a pass that loaded nothing new and stopped, so the X9 blades booted with
# the Intel port alone while stormbootx had just DHCP'd over the Mellanox.
#
# So: core loaded -> its network half, asked for by name. Checked against the
# 6.17 modules: mlx5_core, qede, bnxt_en, ice and i40e each carry their own
# netdev or match a PCI ID, so mlx4 is the one pair today. Loading the half
# before the core's devices exist is fine: it binds them when they appear.
PROTO_HALVES="${STORM_PROTO_HALVES:-mlx4_core:mlx4_en}"
SYS_MODULE="${STORM_SYS_MODULE:-/sys/module}"
MODPROBE="${STORM_MODPROBE:-modprobe}"
for pair in $PROTO_HALVES; do
    core="${pair%%:*}"; half="${pair#*:}"
    [ -d "$SYS_MODULE/$core" ] || continue
    [ -d "$SYS_MODULE/$half" ] && continue
    if $MODPROBE -q "$half" 2>/dev/null; then
        echo "  $half loaded: the network half of $core"
    else
        echo "  WARNING: $core is loaded but $half would not load - its ports will not appear"
    fi
done
# --- END protocol halves

# The ones no device announces: filesystems, and the block driver this image
# exports its root through.
for m in ublk_drv erofs overlay ext4 xfs vfat; do
    modprobe -q "$m" 2>/dev/null || true
done

if [ ! -c /dev/ublk-control ]; then
    echo "WARNING: /dev/ublk-control not found - ublk_drv may not be loaded"
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
if [ "$BOOT_MODE" = "local" ] && [ -z "$IP_CONF" ] && [ -z "$BOOTHOST" ]; then
    # No network asked for. Load ublk and jump straight to the local attach.
    ip link set lo up 2>/dev/null || true
    :
else
echo "Configuring network..."
ip link set lo up
# The instance metadata address.
#
# 169.254.169.254 is where every cloud image asks who it is — cloud-init,
# Afterburn and tinycloudinit all probe it before anything else — and it has
# to exist before the service that answers can bind it.
#
# On loopback, not on a NIC. The address is link-local and identical on every
# cloud by design, so putting it on an interface would answer for it on that
# segment, for machines this node does not run. Guests reach it because the
# node routes to its own loopback.
ip addr add 169.254.169.254/32 dev lo 2>/dev/null || true

# Pick an uplink: carrier first, then speed.
#
# This used to take the first non-loopback interface. "First" is a kernel
# enumeration order, not a statement about which port has a cable in it. On a
# Dell R230 booting over NVMe/TCP it picked eth0 of a two-port Mellanox while
# the cable was in eth1, bridged the dead port, and sat in DHCP for four
# minutes with nothing on the console (stormpump#17). stormbootx, one stage
# earlier and with no drivers at all, had already enumerated the same four
# NICs, filtered to link up, and confirmed one answered before committing.
# Running after Linux has enumerated the same hardware, this should not know
# less than the firmware did.
#
# Carrier cannot be read from a down interface, so every candidate is brought
# up first and the link is given time to negotiate. That wait is not a fixed
# sleep: it ends as soon as anything reports carrier, so a machine whose link
# is already up does not pay for one that is slow.
# --- BEGIN netdev wait (covered by tests/initramfs-netdev.sh)
# Every NIC a driver took has to be a network interface before the ports are
# counted (#250). A driver can bind a PCI function and make its netdev
# seconds later - mlx4_en's ports appear only once mlx4_core has finished
# with the card's firmware - and the uplink selection below enumerates once.
# So wait, bounded, until every network-class PCI function with a driver has
# a netdev under it, walking the auxiliary bus's modaliases each second for a
# split driver whose device came late. A function that never gets one is
# named: a ConnectX-3 port set to InfiniBand is one, and says why the wait
# took its full length.
PCI_SYSFS="${STORM_PCI_SYSFS:-/sys/bus/pci/devices}"
AUX_SYSFS="${STORM_AUX_SYSFS:-/sys/bus/auxiliary/devices}"
NETDEV_WAIT="${STORM_NETDEV_WAIT:-15}"
netdev_missing() {
    for _d in "$PCI_SYSFS"/*; do
        case "$(cat "$_d/class" 2>/dev/null)" in 0x02*) ;; *) continue ;; esac
        [ -e "$_d/driver" ] || continue
        [ -n "$(ls "$_d/net" 2>/dev/null)" ] && continue
        printf '%s ' "$(basename "$_d")"
    done
}
nd_waited=0
MISSING_NET=$(netdev_missing)
while [ -n "$MISSING_NET" ] && [ "$nd_waited" -lt "$NETDEV_WAIT" ]; do
    [ "$nd_waited" -eq 0 ] && echo "  waiting for the network interfaces of: $MISSING_NET"
    for _ma in "$AUX_SYSFS"/*/modalias; do
        [ -f "$_ma" ] && ${MODPROBE:-modprobe} -q "$(cat "$_ma")" 2>/dev/null
    done
    sleep 1
    nd_waited=$((nd_waited + 1))
    MISSING_NET=$(netdev_missing)
done
if [ -n "$MISSING_NET" ]; then
    echo "  WARNING: no network interface after ${nd_waited}s for: $MISSING_NET"
    for _d in $MISSING_NET; do
        echo "    $_d driver $(basename "$(readlink "$PCI_SYSFS/$_d/driver" 2>/dev/null)") $(cat "$PCI_SYSFS/$_d/modalias" 2>/dev/null)"
    done
elif [ "$nd_waited" -gt 0 ]; then
    echo "  every network port is an interface after ${nd_waited}s"
fi
# --- END netdev wait

# --- BEGIN node state read (covered by tests/initramfs-boot-nic.sh)
# What this machine's installed system says about itself, read before the
# network (#229, #238): `/config/stormcos.toml` and `/config/install-node.toml`
# (#78) on the local disk's `stormcos-state` volume. The boot NIC's declared
# address and the name DHCP is asked under both come from here.
#
# Before the network `$SLAB` is set only by `rd.stormblock.slab=`, so the
# disk is looked for: the one the command line names, when it names one (and
# only that one, #273); otherwise the first internal, non-removable disk that
# holds the volume. Never a drive in a shelf, and never a fabric URI (there is
# no network yet). Read-only: `slab cat` opens nothing for writing.
STATE_DIR="${STORM_RUN:-/run}/stormblock-node-state"
STATE_TOML="$STATE_DIR/stormcos.toml"
STATE_NODE="$STATE_DIR/install-node.toml"
STATE_DISK=""
_ss="${STORM_STATE_SYS:-/sys/block}"
mkdir -p "$STATE_DIR" 2>/dev/null || true
rm -f "${STATE_TOML:?}" "${STATE_NODE:?}"
state_read() { # disk -> 0 when it holds stormcos-state (either file may be absent)
    _held=1
    for _f in stormcos.toml install-node.toml; do
        "${STORM_STORMBLOCK:-/usr/sbin/stormblock}" slab cat --slab "$1" --volume stormcos-state \
            --out "$STATE_DIR/$_f" "/config/$_f" >/dev/null 2>&1
        case $? in
        0) _held=0 ;;
        1) _held=0; rm -f "${STATE_DIR:?}/${_f:?}" ;;   # the volume is there, the file is not
        *) rm -f "${STATE_DIR:?}/${_f:?}"; return 1 ;;  # not a slab, or no such volume
        esac
    done
    return $_held
}
state_disks() {
    case "$SLAB" in
    *://*) return 0 ;;
    /*) [ -e "$SLAB" ] && echo "$SLAB"; return 0 ;;
    esac
    for _d in "$_ss"/sd? "$_ss"/sd?? "$_ss"/nvme*n? "$_ss"/nvme*n?? "$_ss"/vd?; do
        [ -e "$_d" ] || continue
        [ "$(cat "$_d/removable" 2>/dev/null)" = "1" ] && continue
        if [ "${ALLOW_EXTERNAL:-}" != 1 ]; then
            case "$(readlink -f "$_d" 2>/dev/null)" in */expander-*) continue ;; esac
            _encl=""
            for _e in "$_d"/device/enclosure_device:*; do [ -e "$_e" ] && _encl=1; done
            [ -n "$_encl" ] && continue
        fi
        echo "${STORM_STATE_DEV:-/dev}/${_d##*/}"
    done
}
for _sd in $(state_disks); do
    if state_read "$_sd"; then
        STATE_DISK="$_sd"
        break
    fi
done
if [ -n "$STATE_DISK" ]; then
    echo "  node state: stormcos-state on $STATE_DISK ($(cd "$STATE_DIR" && ls | tr '\n' ' '))"
fi
# --- END node state read

# --- BEGIN uplink selection (covered by tests/initramfs-nic-selection.sh)
LINK_WAIT="${STORM_LINK_WAIT:-10}"
# Injectable only so the selection can be tested against a fake tree; nothing
# on a node ever sets it.
NET_SYSFS="${NET_SYSFS:-/sys/class/net}"

net_speed() {
    _s=$(cat "$NET_SYSFS/$1/speed" 2>/dev/null) || _s=0
    case "$_s" in ''|*[!0-9]*) _s=0 ;; esac
    echo "$_s"
}

net_carrier() {
    cat "$NET_SYSFS/$1/carrier" 2>/dev/null || echo 0
}

# Physical ports only. A real one has a device symlink, which skips lo, the
# bridge this script is about to create, and any veth or ublk device.
ALL_PHYS=""
for dev in "$NET_SYSFS"/*; do
    name=$(basename "$dev")
    [ "$name" = "lo" ] && continue
    [ -e "$dev/device" ] || continue
    ALL_PHYS="$ALL_PHYS $name"
    ip link set "$name" up 2>/dev/null || true
done

waited=0
while [ "$waited" -lt "$LINK_WAIT" ]; do
    for name in $ALL_PHYS; do
        [ "$(net_carrier "$name")" = "1" ] && break 2
    done
    sleep 1
    waited=$((waited + 1))
done

# What it saw, always — on a machine that will not boot this console is all
# anyone gets, and "no network" without the evidence is not a diagnosis.
if [ -n "$ALL_PHYS" ]; then
    echo "  interfaces after ${waited}s:"
    for name in $ALL_PHYS; do
        echo "    $name  mac $(cat "$NET_SYSFS/$name/address" 2>/dev/null)  speed $(net_speed "$name")  carrier $(net_carrier "$name")"
    done
fi

# Fastest first, same rule stormbootx uses.
CANDIDATES=$(
    for name in $ALL_PHYS; do
        [ "$(net_carrier "$name")" = "1" ] || continue
        printf '%012d %s\n' "$(net_speed "$name")" "$name"
    done | sort -r | awk '{print $2}'
)

if [ -z "$CANDIDATES" ] && [ -n "$ALL_PHYS" ]; then
    # Nothing reported a cable. Still try: carrier can be wrong, and a node
    # that refuses to try is worse than one that fails a DHCP and says so.
    echo "WARNING: no interface reported carrier after ${LINK_WAIT}s - trying all of them"
    CANDIDATES="$ALL_PHYS"
fi

IFACE=$(echo $CANDIDATES | awk '{print $1}')
# --- END uplink selection

if [ -z "$IFACE" ]; then
    if [ "$BOOT_MODE" = "local" ]; then
        # The root is on this disk; the network is for what runs later. A node
        # that boots without an address is degraded and can be looked at. One
        # that drops to a shell in the initramfs cannot be looked at at all.
        echo "WARNING: no network interface found - continuing without one"
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
        rescue_shell
    fi
fi
if [ -z "${NO_NETWORK:-}" ]; then

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

# What the network step decided, written where the booted system can read it.
#
# Everything here is echoed to the console, and on a machine whose console is
# not wired to anything that is the same as not saying it. A node came up with
# its address on the raw uplink and no bridge, Cilium died for want of
# `stormbr0`, and there was no way to find out *why* without rebooting with a
# serial capture — which the BIOS was not redirecting anyway.
#
# `/run` survives the switch_root, so this is readable from the running system
# for as long as it matters.
NETLOG=/run/stormblock/network.log
mkdir -p /run/stormblock 2>/dev/null || true
: > "$NETLOG" 2>/dev/null || true
netsay() {
    echo "$@"
    echo "$@" >> "$NETLOG" 2>/dev/null || true
}

# Bond the uplinks that are alike, so a node with two cables has two paths.
#
# A node was running on one port with a second cabled and idle, and `bond0`
# existed, was down, and had no members — because loading the `bonding` module
# creates one empty bond by default and nothing had ever put anything in it.
# Two cables into a machine mean somebody intended redundancy.
#
# **active-backup by default, and that is a safety decision rather than a
# preference.** 802.3ad needs a LAG configured on the switch; point an LACP
# bond at a switch that has none and the ports do not come up reliably — on
# the one step of the boot that can strand a node, from an initramfs with no
# way to ask. active-backup needs nothing of the switch, survives one cable
# being pulled, and is right on any switch anyone might plug this into.
# `rd.stormblock.bond=802.3ad` asks for the other, deliberately, on a node
# whose switch is known.
#
# `rd.stormblock.bond=off` turns it off entirely.
BOND="${STORM_BOND_DEV:-bond0}"
# **Off by default, until it has earned a default.**
#
# It broke the network twice in one evening and in two different ways, and it
# is not load-bearing: every release before it ran on a single uplink. A
# feature that costs a node its pod network is not one to leave on while it is
# still being proven, so it is opt-in with `rd.stormblock.bond=active-backup`
# (or `802.3ad` on a switch with a LAG) and the single-uplink path — the one
# that has worked all along — is what a node does unless told otherwise.
BOND_MODE="${BOND_MODE:-off}"

# Which uplinks to bond: the ones at the top speed, and only those.
#
# Bonding a 10G port with a 1G one gives a 1G bond in active-backup and a
# confusing one in 802.3ad. A slow port is a fallback, not a peer.
net_bond_members() {
    _top=""
    for _c in $CANDIDATES; do
        _s=$(net_speed "$_c")
        [ -z "$_top" ] && _top="$_s"
        [ "$_s" = "$_top" ] && printf '%s ' "$_c"
    done
}

# Build the bond. Prints the device on success, nothing on failure.
net_make_bond() {
    _members="$(net_bond_members)"
    set -- $_members
    # One port is not a bond. Two cables are the reason this exists.
    [ $# -ge 2 ] || return 1

    # The module may have made an empty bond0 already; reuse it rather than
    # fight it, but take it down first — mode cannot be set on a live bond,
    # and a mode that silently did not apply is worse than no bond.
    ip link add name "$BOND" type bond 2>/dev/null || true
    ip link set "$BOND" down 2>/dev/null || true
    if ! echo "$BOND_MODE" > "/sys/class/net/$BOND/bonding/mode" 2>/dev/null; then
        echo "WARNING: $BOND does not take mode $BOND_MODE" >&2
        return 1
    fi
    # Without a link monitor a bond never notices a cable being pulled, which
    # is the entire thing it was made for.
    echo 100 > "/sys/class/net/$BOND/bonding/miimon" 2>/dev/null || true

    _joined=0
    for _m in $_members; do
        # A port must be down to be enslaved, and must carry no address of
        # its own once it is.
        ip addr flush dev "$_m" 2>/dev/null || true
        ip link set "$_m" down 2>/dev/null || true
        if ip link set "$_m" master "$BOND" 2>/dev/null; then
            _joined=$((_joined + 1))
        else
            echo "WARNING: $_m would not join $BOND" >&2
            ip link set "$_m" up 2>/dev/null || true
        fi
    done
    if [ "$_joined" -lt 2 ]; then
        echo "WARNING: only $_joined port(s) joined $BOND - not bonding" >&2
        net_unbond
        return 1
    fi
    ip link set "$BOND" up 2>/dev/null || { net_unbond; return 1; }
    echo "  bonded: $_members as $BOND ($BOND_MODE)" >&2
    printf '%s' "$BOND"
}

# Put it back exactly as it was, so a failed bond costs nothing.
net_unbond() {
    for _m in $(net_bond_members); do
        ip link set "$_m" nomaster 2>/dev/null || true
        ip link set "$_m" up 2>/dev/null || true
    done
    ip link set "$BOND" down 2>/dev/null || true
    ip link del "$BOND" 2>/dev/null || true
}

# --- BEGIN bridge bring-up (covered by ci-boot-nic-verify.sh)
# Put one uplink on the bridge and leave $IFACE naming whatever now holds the
# address. A function because a lease has to be able to fail and be retried on
# the next candidate, and each attempt has to start from the same state.
net_bring_up() {
    _up="$1"
    ip link set "$_up" up
    IFACE="$_up"
    # Load the bridge module before asking for a bridge.
    #
    # `ip link add type bridge` does not autoload it here, and the kernel then
    # answers **"RTNETLINK answers: Not supported"** — which reads like the
    # kernel lacking bridge support rather than a module nobody loaded. The
    # node came up with its address on the raw uplink, no `stormbr0`, and
    # Cilium dead at "unable to determine direct routing device", because it
    # is configured with `devices: stormbr0`.
    #
    # `bridge.ko` is in this initramfs — `kernel/net/bridge/` is shipped — so
    # this is a modprobe that was never called, not a module that is missing.
    # Silent and unconditional: it is already loaded on a kernel that builds
    # it in, and a failure here shows up as the warning below with the real
    # reason attached.
    modprobe bridge 2>/dev/null || true
    # Create the bridge, or use the one that is already there.
    #
    # This was `ip link add … && …`, which is false when the bridge *exists* —
    # so a second call, after a first attempt had created it and a teardown
    # had not fully removed it, silently skipped bridging altogether. The
    # address then went straight onto the uplink, there was no `stormbr0`, and
    # Cilium — which is configured with `devices: stormbr0` because
    # auto-detection skips bridges — died at "unable to determine direct
    # routing device". A node with a working DHCP lease and no pod network.
    if [ -n "${NO_BRIDGE:-}" ]; then
        netsay "  no bridge: NO_BRIDGE is set"
    elif ! ip link add name "$BRIDGE" type bridge 2>/dev/null \
         && [ ! -d "/sys/class/net/$BRIDGE" ]; then
        # Neither created nor already present: say which, because a node with
        # no bridge has no pod network and this is the only place that knows.
        netsay "WARNING: could not create $BRIDGE ($(ip link add name "$BRIDGE" type bridge 2>&1 | head -1)) - no VM networking"
    fi
    if [ -z "${NO_BRIDGE:-}" ] && [ -d "/sys/class/net/$BRIDGE" ]; then
        if ip link set "$_up" master "$BRIDGE" 2>/dev/null && ip link set "$BRIDGE" up; then
            # Everything below configures the bridge instead: it is the
            # interface that now holds the address, and the uplink is one of
            # its ports.
            netsay "  bridged: $_up is a port of $BRIDGE"
            IFACE="$BRIDGE"
        else
            netsay "WARNING: could not enslave $_up to $BRIDGE - no VM networking"
            ip link del "$BRIDGE" 2>/dev/null || true
        fi
    fi
}

# Undo a failed attempt, so the next candidate starts clean rather than
# inheriting a bridge with a dead port still in it.
net_teardown() {
    ip addr flush dev "$BRIDGE" 2>/dev/null || true
    ip link set "$1" nomaster 2>/dev/null || true
    ip link del "$BRIDGE" 2>/dev/null || true
    ip addr flush dev "$1" 2>/dev/null || true
}
# --- END bridge bring-up

# --- BEGIN boot nic (covered by tests/initramfs-boot-nic.sh)
# Which port the node's address goes on, and whether it is static (#229).
#
# Every release's command line says `ip=dhcp` (one line boots every node,
# stormcos#182), so what one node declares has to win here: stormpump never
# addresses a bridge port after switch_root (stormpump#33), and the boot NIC
# is one. The declaration is read the way stormpump's `plan_for` reads it:
# stormcos.toml when it declares any interface, else install-node.toml (#78),
# whole, never merged. Its `[network]` single form is the node's primary
# interface, the one it boots on:
#   static, an exact name, present, with carrier -> its addresses, gateway,
#     dns, domain and mtu, on the bridge, instead of DHCP;
#   dhcp with an exact name -> that port is tried first.
# A declared port with no carrier, or not on this machine, is said and the
# boot goes on with DHCP as before: a node that cannot reach its appliance
# cannot boot. A static `ip=` on the command line wins over the declaration,
# and its `<device>` field names the port. `rd.stormblock.declared-net=off`
# ignores the declaration. Values are read from one line each (arrays on one
# line), which is how stormpump writes install-node.toml.
BOOT_NIC=""
BOOT_ADDRS=""
BOOT_GW=""
BOOT_DNS=""
BOOT_DOMAIN=""
BOOT_MTU=""
BOOT_FROM=""
nic_present() { # name -> 0 when it is one of this machine's ports
    for _p in $ALL_PHYS; do [ "$_p" = "$1" ] && return 0; done
    return 1
}
mask_prefix() { # 24 | 255.255.255.0 -> 24; nothing when it is neither
    case "$1" in
    *.*.*.*) echo "$1" | awk -F. '{
        n = 0
        for (i = 1; i <= 4; i++) { v = $i + 0; while (v > 0) { n += v % 2; v = int(v / 2) } }
        print n }' ;;
    *[!0-9]*|'') ;;
    *) echo "$1" ;;
    esac
}
toml_declares() { # file -> 0 when it declares any interface (stormpump's rule)
    [ -s "$1" ] || return 1
    awk '
        /^[[:space:]]*\[\[[[:space:]]*network\.interfaces[[:space:]]*\]\]/ { s = 2; next }
        /^[[:space:]]*\[[[:space:]]*network[[:space:]]*\][[:space:]]*(#.*)?$/ { s = 1; next }
        /^[[:space:]]*\[/ { s = 0; next }
        s == 1 && /^[[:space:]]*(interface|name)[[:space:]]*=/ { f = 1 }
        s == 2 && /^[[:space:]]*(name|driver)[[:space:]]*=/ { f = 1 }
        END { exit !f }' "$1"
}
toml_network() { # file key -> the value under [network], quotes and brackets gone
    awk -v want="$2" '
        /^[[:space:]]*\[/ { s = ($0 ~ /^[[:space:]]*\[[[:space:]]*network[[:space:]]*\][[:space:]]*(#.*)?$/); next }
        s && /=/ {
            k = $0; sub(/[[:space:]]*=.*/, "", k); gsub(/[[:space:]]/, "", k)
            if (k != want) next
            v = $0; sub(/^[^=]*=[[:space:]]*/, "", v); sub(/[[:space:]]*#.*$/, "", v)
            gsub(/[]["\047,]/, " ", v); gsub(/[[:space:]]+/, " ", v); sub(/^ /, "", v); sub(/ $/, "", v)
            print v; exit
        }' "$1"
}

# The command line first: a static `ip=<addr>::<gw>:<mask>::<device>:none`.
_ipa=$(echo "$IP_CONF" | cut -d: -f1)
_ipdev=$(echo "$IP_CONF" | cut -d: -f6)
case "$IP_CONF" in
''|dhcp|on|any|dhcp6|auto6|off|none) _ipa="" ;;
esac
DECL_FILE=""
DECL_FROM=""
if [ "${DECLARED_NET:-}" != off ]; then
    if toml_declares "${STATE_TOML:-}"; then
        DECL_FILE="$STATE_TOML"; DECL_FROM="stormcos.toml on ${STATE_DISK:-the local disk}"
    elif toml_declares "${STATE_NODE:-}"; then
        DECL_FILE="$STATE_NODE"; DECL_FROM="install-node.toml on ${STATE_DISK:-the local disk}"
    fi
elif toml_declares "${STATE_TOML:-}" || toml_declares "${STATE_NODE:-}"; then
    netsay "  boot NIC: the node's declared network is ignored (rd.stormblock.declared-net=off)"
fi

if [ -n "$_ipa" ]; then
    _ipgw=$(echo "$IP_CONF" | cut -d: -f3)
    _ippre=$(mask_prefix "$(echo "$IP_CONF" | cut -d: -f4)")
    BOOT_NIC="$IFACE"
    if [ -n "$_ipdev" ]; then
        if nic_present "$_ipdev"; then
            BOOT_NIC="$_ipdev"
        else
            netsay "WARNING: ip= names $_ipdev, which this machine does not have (ports:$ALL_PHYS) - using $IFACE"
        fi
    fi
    case "$_ipa" in */*) BOOT_ADDRS="$_ipa" ;; *) BOOT_ADDRS="$_ipa${_ippre:+/$_ippre}" ;; esac
    BOOT_GW="$_ipgw"
    BOOT_FROM="the command line (ip=)"
    [ -n "$DECL_FILE" ] && netsay "  boot NIC: ip= on the command line wins over the declaration in $DECL_FROM"
elif [ -n "$DECL_FILE" ]; then
    _dn=$(toml_network "$DECL_FILE" name)
    [ -n "$_dn" ] || _dn=$(toml_network "$DECL_FILE" interface)
    _dm=$(toml_network "$DECL_FILE" mode)
    _da="$(toml_network "$DECL_FILE" address) $(toml_network "$DECL_FILE" addresses)"
    case "${_dm:-dhcp}" in
    static|dhcp) ;;
    *) _dn="" ;;   # `up`: something else addresses it; nothing for the boot to do
    esac
    case "$_dn" in
    '') ;;
    *'*'*)
        netsay "  boot NIC: [network] in $DECL_FROM names '$_dn', a pattern, not one port - the usual selection"
        ;;
    *)
        if ! nic_present "$_dn"; then
            netsay "WARNING: [network] in $DECL_FROM names $_dn, which this machine does not have (ports:$ALL_PHYS) - DHCP as usual"
        elif [ "${_dm:-dhcp}" = dhcp ]; then
            if [ "$(net_carrier "$_dn")" = 1 ]; then
                CANDIDATES="$_dn $(for _c in $CANDIDATES; do [ "$_c" = "$_dn" ] || printf '%s ' "$_c"; done)"
                CANDIDATES="${CANDIDATES% }"
                IFACE="$_dn"
                netsay "  boot NIC: $_dn first, as [network] in $DECL_FROM declares (dhcp)"
            else
                netsay "WARNING: [network] in $DECL_FROM names $_dn, which has no carrier - DHCP on the others"
            fi
        else
            _good=""
            for _a in $_da; do
                case "$_a" in
                */[0-9]*) _good="$_good $_a" ;;
                *) netsay "WARNING: [network] in $DECL_FROM: address '$_a' states no prefix - not applied" ;;
                esac
            done
            _good="${_good# }"
            if [ -z "$_good" ]; then
                netsay "WARNING: [network] in $DECL_FROM is static on $_dn with no usable address - DHCP as usual"
            elif [ "$(net_carrier "$_dn")" != 1 ]; then
                # Static on a port with no cable is a node nobody can reach,
                # and one that cannot reach its appliance to boot. DHCP
                # elsewhere is at least a node that can be looked at.
                netsay "WARNING: [network] in $DECL_FROM puts $_good on $_dn, which has no carrier - DHCP on the others instead"
            else
                BOOT_NIC="$_dn"
                BOOT_ADDRS="$_good"
                BOOT_GW=$(toml_network "$DECL_FILE" gateway)
                BOOT_DNS=$(toml_network "$DECL_FILE" dns)
                BOOT_DOMAIN=$(toml_network "$DECL_FILE" domain)
                BOOT_MTU=$(toml_network "$DECL_FILE" mtu)
                case "$BOOT_MTU" in *[!0-9]*) BOOT_MTU="" ;; esac
                BOOT_FROM="[network] in $DECL_FROM"
            fi
        fi
        ;;
    esac
fi
# Put the static address on: the port onto the bridge (net_bring_up), the
# addresses on whatever holds them, the gateway, and the resolver a lease would
# have written, so appliance discovery and the node name read the same files.
static_apply() {
    # A static address: from the command line, or declared by the node (the
    # boot nic block). One port only: the address was chosen for a particular
    # port, and putting it on a different one is not a fallback, it is a
    # wrong answer.
    netsay "  static: $BOOT_ADDRS on $BOOT_NIC${BOOT_GW:+, gateway $BOOT_GW}, from $BOOT_FROM"
    if [ -n "$BOOT_MTU" ]; then
        ip link set "$BOOT_NIC" mtu "$BOOT_MTU" 2>/dev/null \
            || netsay "WARNING: $BOOT_NIC does not take mtu $BOOT_MTU"
    fi
    net_bring_up "$BOOT_NIC"
    for _a in $BOOT_ADDRS; do
        ip addr add "$_a" dev "$IFACE" || netsay "WARNING: could not add $_a to $IFACE"
    done
    if [ -n "$BOOT_GW" ]; then
        ip route add default via "$BOOT_GW" || netsay "WARNING: no default route via $BOOT_GW"
    fi
    # What a lease would have written, so appliance discovery and the node
    # name read the same files either way.
    if [ -n "$BOOT_DNS$BOOT_DOMAIN" ]; then
        : > /etc/resolv.conf
        [ -n "$BOOT_DOMAIN" ] && echo "search $BOOT_DOMAIN" >> /etc/resolv.conf
        for _d in $BOOT_DNS; do echo "nameserver $_d" >> /etc/resolv.conf; done
    fi
    [ -n "$BOOT_DOMAIN" ] && echo "$BOOT_DOMAIN" > /run/dhcp-domain
    [ -n "$BOOT_DNS" ] && echo "$BOOT_DNS" > /run/dhcp-dns
}
# --- END boot nic

if [ -n "$BOOT_ADDRS" ]; then
    static_apply
else
    # DHCP, over each candidate in turn until one answers.
    #
    # Carrier says a cable is in the port. It does not say the port reaches a
    # DHCP server — a link to a switch with no route to one has carrier and
    # no lease. So a lease is the actual test, and failing it moves to the
    # next candidate instead of falling to link-local with three good ports
    # untried.
    #
    # udhcpc does not configure anything itself: it obtains a lease and hands
    # the values to a script, and everything an interface needs happens there.
    # This was `-s /bin/true`, so every boot took a lease from the server —
    # visible in the server's lease table, which made it look like it had
    # worked — and applied none of it. The node came up with an address
    # allocated to it and no address on it.
    #
    # Fewer retries per candidate when there is more than one, so trying four
    # ports does not take four times as long as trying the only one there is.
    DHCP_TRIES=5
    [ "$(echo $CANDIDATES | wc -w)" -le 1 ] && DHCP_TRIES=10

    # --- BEGIN dhcp name hint (covered by tests/initramfs-node-name.sh)
    # The name this node asks for its lease under (option 12, #238), so the
    # server's lease table says who holds each lease. Before the lease the
    # network has not said what the node is called, so the hint comes from
    # what this machine already knows:
    #   1. its declared name, `[node] hostname` in /config/stormcos.toml, else
    #      install-node.toml, on the local disk's `stormcos-state` volume (a
    #      node installed before; copied by the node state read block, #229);
    #   2. the name its firmware claimed its boot image on (`StormBootTag`,
    #      #249), which stormbootx took from DHCP and reverse DNS a stage
    #      earlier. Never an SMBIOS guess, and never a `mac-` placeholder.
    # Without one, nothing is sent and the server names the lease as before.
    DHCP_NAME=""
    DHCP_NAME_FROM=""
    DHCP_HOST_ARGS=""
    # Read by the node state read block; stormcos.toml wins, as in stormpump.
    for _hint_toml in "${STATE_TOML:-}" "${STATE_NODE:-}"; do
        [ -s "$_hint_toml" ] || continue
        DHCP_NAME=$(awk '
            /^[[:space:]]*\[/ { s = ($0 ~ /^[[:space:]]*\[node\][[:space:]]*(#.*)?$/) ; next }
            s && /^[[:space:]]*hostname[[:space:]]*=/ {
                sub(/^[^=]*=[[:space:]]*/, ""); sub(/[[:space:]]*#.*$/, ""); gsub(/"/, ""); print; exit
            }' "$_hint_toml")
        if [ -n "$DHCP_NAME" ]; then
            DHCP_NAME_FROM="its declared name (${_hint_toml##*/} on ${STATE_DISK:-the local disk})"
            break
        fi
    done
    if [ -z "$DHCP_NAME" ] && [ "${BOOTTAG_FROM:-}" = firmware ]; then
        case "$BOOTTAG" in
        ""|mac-*) ;;
        *) DHCP_NAME="$BOOTTAG"; DHCP_NAME_FROM="the name its firmware booted as" ;;
        esac
    fi
    # A hostname, short: letters, digits and dashes (RFC 1123), or nothing.
    DHCP_NAME=$(printf '%s' "${DHCP_NAME%%.*}" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9-' '-' | sed 's/^-*//; s/-*$//')
    if [ -n "$DHCP_NAME" ]; then
        DHCP_HOST_ARGS="-x hostname:$DHCP_NAME"
        echo "  asking DHCP as '$DHCP_NAME', $DHCP_NAME_FROM"
    fi
    # --- END dhcp name hint

    # The bond first, when there is one to make.
    #
    # Tried ahead of the single ports and falls back to them: a bond that
    # cannot get a lease is a bond pointed at a switch that is not expecting
    # one, and the answer to that is the port that worked before, not a node
    # that will not boot.
    LEASED=""
    if [ "$BOND_MODE" != "off" ] && [ -z "${NO_BRIDGE:-}" ]; then
        BONDED=$(net_make_bond) || BONDED=""
        # Only if it named a real interface.
        #
        # Belt and braces after a warning printed to stdout inside that
        # function was captured as the device name, and the boot then tried to
        # bring up an interface called "WARNING: ...". Anything that is not a
        # device in /sys is not a device, whatever it says.
        if [ -n "$BONDED" ] && [ ! -d "/sys/class/net/$BONDED" ]; then
            echo "WARNING: the bond step returned '$BONDED', which is not an interface" >&2
            BONDED=""
            net_unbond
        fi
        if [ -n "$BONDED" ]; then
            net_bring_up "$BONDED"
            if udhcpc -i "$IFACE" -s /usr/share/udhcpc/default.script -q -n -t "$DHCP_TRIES" $DHCP_HOST_ARGS; then
                LEASED="$BONDED"
            else
                echo "  no lease on $BONDED - falling back to single ports"
                net_teardown "$BONDED"
                net_unbond
            fi
        fi
    fi

    [ -n "$LEASED" ] || for UPLINK in $CANDIDATES; do
        netsay "  trying $UPLINK (speed $(net_speed "$UPLINK"), carrier $(net_carrier "$UPLINK"))"
        net_bring_up "$UPLINK"
        if udhcpc -i "$IFACE" -s /usr/share/udhcpc/default.script -q -n -t "$DHCP_TRIES" $DHCP_HOST_ARGS; then
            LEASED="$UPLINK"
            break
        fi
        netsay "  no lease on $UPLINK"
        net_teardown "$UPLINK"
    done

    if [ -z "$LEASED" ]; then
        # Name every port that was tried. The old message said only "DHCP
        # failed", which on a four-port machine does not say whether it tried
        # one port or all of them — the difference between a dead switch and
        # a cable in the wrong socket.
        echo "WARNING: DHCP failed on every candidate ($CANDIDATES), trying link-local..."
        net_bring_up "$(echo $CANDIDATES | awk '{print $1}')"
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
    # names its machines has already decided. Then DNS, which holds the same
    # decision in the other direction. Only then the MAC — the one identifier
    # a machine has before anyone has told it anything, and a name nobody
    # chose.
    # --- BEGIN node name (covered by tests/initramfs-node-name.sh)
    # Every step says why it gave no name (#238): C2NR0Q2 registered as
    # `storm-06f96d` with a reservation and a confirmed PTR naming it
    # `stormblock1`, and nothing on the console said which step had failed.
    _nr="${STORM_NAME_RUN:-/run}"
    _resolv="${STORM_RESOLV:-/etc/resolv.conf}"
    NODE_NAME=""
    NODE_DOMAIN="$(cat "$_nr/dhcp-domain" 2>/dev/null || true)"
    _opt12="$(cat "$_nr/dhcp-hostname" 2>/dev/null || true)"
    if [ -n "$_opt12" ]; then
        NODE_NAME="${_opt12%%.*}"
        # A server that sends the FQDN in option 12 has said the domain too.
        case "$_opt12" in *.*) [ -n "$NODE_DOMAIN" ] || NODE_DOMAIN="${_opt12#*.}" ;; esac
        echo "  name from DHCP: $NODE_NAME (option 12)"
    else
        echo "  the lease names no host (no option 12)"
    fi

    # Then ask DNS what this address is called.
    #
    # A name is a fact about a network, not about a machine, and the network
    # already holds it: the address this node was just leased has a PTR. A
    # site that names its machines has named this one, and asking is how the
    # node finds out — `storm-<mac>` below is a name nobody chose, which the
    # node made up because it had not asked.
    #
    # The short form, not the FQDN: the domain travels separately, and a
    # hostname carrying it turns up doubled in every certificate subject and
    # log line that appends one.
    if [ -z "$NODE_NAME" ]; then
        MYIP=$(ip -4 -o addr show dev "$IFACE" 2>/dev/null \
               | awk '{ print $4 }' | cut -d/ -f1 | head -1)
        if [ -z "$MYIP" ]; then
            echo "  no IPv4 address on $IFACE: cannot ask DNS what this node is called"
        elif ! grep -q '^nameserver' "$_resolv" 2>/dev/null; then
            # The lease carried no option 6 (microdns#14 on the g16 pool).
            echo "  no DNS server in the lease: cannot ask DNS what $MYIP is called"
        else
            PTRNAME=$(nslookup "$MYIP" 2>/dev/null \
                      | sed -n 's/.*name = \(.*\)\.$/\1/p' | head -1)
            # Forward-confirmed, or not at all.
            #
            # A reverse record outlives whatever held the address. This pool
            # hands out 192.168.30.1 and its PTR still said
            # `minint-fsmpc1o.g16.lo` hours after a stormcos node had the
            # lease — a Windows box that held it previously, whose own A
            # record points at .2. Taking a name on the strength of a PTR
            # alone would have named this node after that machine.
            #
            # So the name has to round-trip: whatever the PTR says must
            # resolve back to the address asking. A stale record fails that,
            # and the node falls through to naming itself.
            if [ -z "$PTRNAME" ]; then
                echo "  DNS has no name for $MYIP (no PTR record from $(awk '/^nameserver/ { printf "%s%s", sep, $2; sep = " " }' "$_resolv" 2>/dev/null))"
            else
                # Among its addresses, not equal to the last of them.
                #
                # A node with two NICs on one network has one name and two A
                # records, which is the ordinary arrangement and not a
                # mistake: `stormblock1.g16.lo` answers 192.168.30.1 and
                # 192.168.30.2. Membership is the question forward-confirmed
                # reverse DNS actually asks: does the name the PTR gave resolve
                # back to the address that asked.
                BACK=$(nslookup "$PTRNAME" 2>/dev/null \
                       | awk '/^Address: /{ print $2 }')
                if printf '%s\n' "$BACK" | grep -qxF "$MYIP"; then
                    NODE_NAME=${PTRNAME%%.*}
                    case "$PTRNAME" in *.*) [ -n "$NODE_DOMAIN" ] || NODE_DOMAIN="${PTRNAME#*.}" ;; esac
                    echo "  name from DNS: $NODE_NAME ($MYIP, confirmed)"
                else
                    echo "  DNS calls $MYIP '$PTRNAME', which resolves to '$(printf '%s' "${BACK:-nothing}" | tr '\n' ' ')' - ignoring"
                fi
            fi
        fi
    fi

    if [ -z "$NODE_NAME" ]; then
        # The *uplink's* MAC, not the bridge's: a bridge takes a random
        # address until it has a port, so naming a node after it would give
        # the same machine a different name every boot.
        MAC="$(cat "${STORM_SYSNET:-/sys/class/net}/${UPLINK:-$IFACE}/address" 2>/dev/null | tr -d ':')"
        if [ -n "$MAC" ]; then
            NODE_NAME="storm-$(echo "$MAC" | tail -c 7)"
            echo "  nothing named this node: $NODE_NAME, made up from the MAC"
        fi
    fi
    if [ -n "$NODE_NAME" ]; then
        echo "$NODE_NAME" > "${STORM_PROC_SYS:-/proc/sys}/kernel/hostname" 2>/dev/null || true
        echo "  hostname: $NODE_NAME"
        if [ -n "$NODE_DOMAIN" ]; then
            # The node can state its FQDN: the kernel keeps the domain across
            # switch_root, and the running node reads it from there (#238).
            echo "$NODE_DOMAIN" > "${STORM_PROC_SYS:-/proc/sys}/kernel/domainname" 2>/dev/null || true
            echo "  fqdn: $NODE_NAME.$NODE_DOMAIN"
        fi
    fi
    # --- END node name
else
    # An empty summary is the symptom that hid a broken DHCP script for as
    # long as it did; say what it means instead of printing a blank.
    echo "WARNING: $IFACE has no address - nothing on this node will be reachable"
fi

# The clock is stepped once below (the clock step block), after this branch,
# from what the lease offered:
#   /run/ntp-servers   DHCP option 42, if any

fi
fi

# The node's own config, copied for the network step: not kept past it.
rm -f "${STATE_TOML:?}" "${STATE_NODE:?}"

# --- BEGIN clock step (covered by tests/initramfs-clock.sh)
# Set the clock once, bounded, before anything reads it (#251).
#
# The X9 blades have no RTC battery: after a power cut the kernel starts at
# whatever the RTC says, which is 2000. The node's own `timesync` keeps the
# clock, but nothing orders it before stormcert's one-shots and fastetcd, and
# they check certificates against it. So the initramfs steps it here, where
# everything after it waits by construction.
#
# It once cost 50 s of a 74 s boot, waiting on a name that would not resolve.
# So: no names. The lease's option-42 servers (addresses by definition), then
# fixed addresses, each try under `timeout`. Worst case is two waits of
# STORM_NTP_WAIT seconds, on a network with no time at all. Never fatal.
#
# A step is written to the RTC, so the next boot of a machine that has one
# starts right. Without a step, a clock before this image was built is
# certainly wrong, and the build date is a better guess than 2000.
#
# stormbootx (v0.12.0+) sets the RTC from NTP itself and says so in a volatile
# EFI variable, StormBootClock (`synced:<server>` or `unsynced`; absent when
# not chain-loaded by it) (#253). When it synced, the clock is not stepped a
# second time, which saves up to STORM_NTP_WAIT seconds per try — unless the
# clock still reads before this image was built, which no synced clock can,
# or rd.stormblock.ntp=always asks for the step anyway. How the clock was set
# is kept in /run/stormblock/clock, one line, for the node.
NTP_FALLBACK="${STORM_NTP_FALLBACK:-162.159.200.1 216.239.35.0}"
NTP_WAIT="${STORM_NTP_WAIT:-3}"
# Wall-clock seconds less uptime: changes only when the clock is set, so the
# difference across a try is the step, not the time the try took.
clock_base() {
    _up=$(cut -d. -f1 "${STORM_UPTIME:-/proc/uptime}" 2>/dev/null)
    echo $(( $(date +%s) - ${_up:-0} ))
}
# servers... -> 0 when ntpd set the clock; CLOCK_FROM = who answered
clock_try() {
    [ $# -gt 0 ] || return 1
    _args=""
    for _s in "$@"; do _args="$_args -p $_s"; done
    # The group's stderr to /dev/null: ash says "Terminated" when timeout
    # kills ntpd, and that is not news for the console.
    { _out=$(timeout "$NTP_WAIT" ntpd -n -q -d $_args 2>&1); _rc=$?; } 2>/dev/null
    [ "$_rc" = 0 ] || return 1
    CLOCK_FROM=$(printf '%s\n' "$_out" | sed -n 's/.*reply from \([^: ]*\).*/\1/p' | tail -1)
    [ -n "$CLOCK_FROM" ] || CLOCK_FROM=$(echo "$*" | tr ' ' ',')
    return 0
}
# What stormbootx said about the clock: "synced <server>", "unsynced" or
# nothing (#253).
clock_firmware() {
    _f="${EFIVARS:-${STORM_EFIVARS:-/sys/firmware/efi/efivars}}/StormBootClock-${STORMBOOT_GUID:-ab361f54-0166-44a4-a088-1ac22e98ab76}"
    [ -r "$_f" ] || return 0
    _v=$(tail -c +5 "$_f" 2>/dev/null | tr -d '\000\n\r ')
    case "$_v" in
        unsynced) echo unsynced ;;
        synced:*[!A-Za-z0-9._:-]*) echo "  ignoring StormBootClock: '$_v' is not a server" >&2 ;;
        synced:?*) echo "synced ${_v#synced:}" ;;
        "") ;;
        *) echo "  ignoring StormBootClock: '$_v'" >&2 ;;
    esac
}
clock_note() { # one line: how this boot's clock was set, for the node (#253)
    _st="${STORM_CLOCK_STATE:-/run/stormblock/clock}"
    mkdir -p "$(dirname "$_st")" 2>/dev/null
    echo "$*" > "$_st" 2>/dev/null || true
}
clock_step() { # $1 = the node's address, empty when there is no network
    CLOCK_FROM=""
    _stepped=""
    _dhcp=$(cat "${STORM_NTP_SERVERS:-/run/ntp-servers}" 2>/dev/null)
    _fw=$(clock_firmware)
    _fwfloor=$(cat "${STORM_BUILD_DATE:-/etc/stormblock/build-date}" 2>/dev/null)
    case "$_fw" in
        synced\ *)
            echo "clock: firmware synced from ${_fw#synced } ($(date -u '+%Y-%m-%d %H:%M:%S') UTC)"
            case "$_fwfloor" in ''|*[!0-9]*) _fwfloor=0 ;; esac
            if [ "${NTP_MODE:-}" = always ]; then
                echo "  stepped again anyway (rd.stormblock.ntp=always)"
            elif [ "$(date +%s)" -lt "$_fwfloor" ]; then
                echo "  WARNING: but the clock reads before this image was built: stepping it"
            else
                clock_note "firmware ${_fw#synced }"
                return 0
            fi ;;
        unsynced) echo "clock: firmware did not sync" ;;
    esac
    case "${NTP_MODE:-}:$1" in
        off:*|0:*|no:*) echo "Clock: not stepped (rd.stormblock.ntp=$NTP_MODE)" ;;
        *:)             echo "Clock: not stepped (no network)" ;;
        *:169.254.*)    echo "Clock: not stepped (link-local address only)" ;;
        *)
            _before=$(clock_base)
            # shellcheck disable=SC2086 # word lists on purpose
            if { [ -n "$_dhcp" ] && clock_try $_dhcp; } || clock_try $NTP_FALLBACK; then
                _d=$(( $(clock_base) - _before ))
                [ "$_d" -ge 0 ] && _d="+$_d"
                echo "clock stepped by $_d s from $CLOCK_FROM ($(date -u '+%Y-%m-%d %H:%M:%S') UTC)"
                clock_note "ntp $CLOCK_FROM"
                if hwclock -w -u >/dev/null 2>&1; then
                    echo "  RTC written (UTC)"
                else
                    echo "  RTC not written (no RTC, or it refused)"
                fi
                _stepped=1
            else
                echo "WARNING: no time server answered within ${NTP_WAIT}s each (${_dhcp:+$_dhcp; }$NTP_FALLBACK)"
            fi ;;
    esac
    [ -n "$_stepped" ] && return 0
    clock_note "unset"
    _floor=$(cat "${STORM_BUILD_DATE:-/etc/stormblock/build-date}" 2>/dev/null)
    case "$_floor" in ''|*[!0-9]*) return 0 ;; esac
    _now=$(date +%s)
    [ "$_now" -lt "$_floor" ] || return 0
    _was=$(date -u -d "@$_now" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || echo "$_now")
    if date -u -s "@$_floor" >/dev/null 2>&1; then
        clock_note "build-date"
        echo "WARNING: ************************************************************"
        echo "WARNING: CLOCK WAS $_was UTC, BEFORE THIS IMAGE WAS BUILT."
        echo "WARNING: SET TO THE BUILD DATE $(date -u '+%Y-%m-%d %H:%M:%S') UTC - NOT THE REAL TIME."
        echo "WARNING: ************************************************************"
    else
        echo "WARNING: clock is $_was UTC, before this image was built, and could not be set"
    fi
    return 0
}
# --- END clock step
clock_step "${NETADDR:-}"
stamp "clock"

# Start stormblock with ublk export
echo "Starting StormBlock..."
if [ "$BOOT_MODE" = "local" ]; then
    # One identity for every connect this boot makes, rather than threading it
    # through each call. Composed by the firmware, echoed here: the format
    # lives in stormbootx and nothing re-derives it.
    if [ -n "$HOSTNQN" ]; then
        export STORMBLOCK_HOST_NQN="$HOSTNQN"
        echo "Host NQN: $HOSTNQN"
    fi

    # Which appliance to ask, when this machine has no slab of its own.
    #
    # Never baked into the image. An address in a pallet member is an image
    # that belongs to one network, and the moment there are two networks it
    # is an image per network - the same mistake as a service tag on the
    # command line, one level up.
    #
    # So the node asks the network it is actually on, and every source it
    # tries is one a network either already provides or can provide without
    # being rebuilt:
    #
    #   rd.stormblock.boothost=   an operator overriding, or a lab with no DHCP
    #   DHCP option 17            root-path, when the lease carries a URL
    #   boothost.<search domain>  one A record, and the name is the same
    #                             everywhere - each network answers with its own
    #   the DHCP next-server      what the lease named for booting
    #   the DHCP server itself    the last thing known to be up and listening
    #
    # None of them is trusted on sight: each is *asked*, and the first that
    # answers as an engine is the appliance. A candidate that is right by
    # coincidence and one that is right by configuration are the same thing
    # to a node that has to boot, and a candidate that is wrong costs three
    # seconds. That is what makes this work on a network nobody prepared:
    # with no record and no option 17, the DHCP server is tried, and on a
    # small network it is very often the appliance.
    # --- BEGIN appliance discovery (covered by tests/initramfs-no-appliance.sh)
    # Why there is no appliance, when there is none: said again wherever it
    # matters (the release check it skips, a disk that cannot boot, #294).
    BOOTHOST_WHY=""
    if [ -z "$BOOTHOST" ]; then
        BOOTPORT="${BOOTPORT:-9090}"
        CANDIDATE_HOSTS=""
        [ -s "${STORM_BOOTHOST_FILE:-/run/stormblock-boothost}" ] \
            && CANDIDATE_HOSTS="$(cat "${STORM_BOOTHOST_FILE:-/run/stormblock-boothost}")"
        # The network named its boothost: it is there to be asked, so a
        # silence is a link that has just come up (server8's ConnectX-3:
        # carrier, then ARP, then forge), not an answer. Ask again for a
        # while before going on without it (#294).
        NAMED_HOST="$CANDIDATE_HOSTS"
        # Qualified from the lease's own search domain rather than left to the
        # resolver: a short name that fails to resolve looks exactly like an
        # appliance that is down, and the two want different answers.
        # Say what is actually known before guessing from it.
        #
        # The last two boots failed here and the message named the candidates
        # without naming what they were built from, so the same wrong theory
        # was fixed twice. A resolver that was never configured and a domain
        # that was never offered produce the same symptom — a name that does
        # not resolve — and they need different fixes.
        echo "  resolver:  $(tr '\n' ' ' < /etc/resolv.conf 2>/dev/null || echo 'no /etc/resolv.conf')"
        echo "  from DHCP: domain='$(cat /run/dhcp-domain 2>/dev/null)' dns='$(cat /run/dhcp-dns 2>/dev/null)'"

        BOOTDOM=$(cat /run/dhcp-domain 2>/dev/null)
        [ -n "$BOOTDOM" ] || BOOTDOM=$(awk '/^search/ { print $2; exit }' /etc/resolv.conf 2>/dev/null)
        # Failing that, ask the resolver what it calls itself. A nameserver's
        # own address is the one piece of DNS configuration every lease
        # carries, and its PTR names the domain it serves — `192.168.31.252`
        # is `dns.g16.lo`, so the domain is there for the asking without the
        # server having to offer option 15 or 119 at all.
        if [ -z "$BOOTDOM" ]; then
            for ns in $(awk '/^nameserver/ { print $2 }' /etc/resolv.conf 2>/dev/null); do
                BOOTDOM=$(nslookup "$ns" 2>/dev/null | sed -n 's/.*name = [^.]*\.\(.*\)\.$/\1/p' | head -1)
                [ -n "$BOOTDOM" ] && { echo "  domain from the resolver's own PTR: $BOOTDOM"; break; }
            done
        fi
        [ -n "$BOOTDOM" ] && CANDIDATE_HOSTS="$CANDIDATE_HOSTS http://boothost.$BOOTDOM:$BOOTPORT"
        CANDIDATE_HOSTS="$CANDIDATE_HOSTS http://boothost:$BOOTPORT"
        for f in /run/dhcp-siaddr /run/dhcp-serverid; do
            [ -s "$f" ] && CANDIDATE_HOSTS="$CANDIDATE_HOSTS http://$(cat "$f"):$BOOTPORT"
        done
        BH_WAIT=0
        [ -n "$NAMED_HOST" ] && BH_WAIT="${STORM_BOOTHOST_WAIT:-90}"
        BH_START=$(date +%s)
        BH_PASS=0
        while :; do
            BH_PASS=$((BH_PASS + 1))
            for c in $CANDIDATE_HOSTS; do
                case "$c" in
                nvme-tcp://*) BOOTHOST="$c"; break ;;
                esac
                # Two paths, because an appliance that predates the health
                # endpoint answers 404 to it and would be passed over. `slabs`
                # is not a health check — it reads state — but it is
                # engine-specific and it exists everywhere, so it settles the
                # question for an appliance that has not been updated yet.
                if wget -q -T 3 -O /dev/null "$c/api/v1/health" 2>/dev/null \
                   || wget -q -T 3 -O /dev/null "$c/api/v1/slabs" 2>/dev/null; then
                    BOOTHOST="$c"
                    break
                fi
                [ "$BH_PASS" = 1 ] && echo "  no engine at $c"
            done
            [ -n "$BOOTHOST" ] && break
            BH_ELAPSED=$(( $(date +%s) - BH_START ))
            [ "$BH_ELAPSED" -ge "$BH_WAIT" ] && break
            [ "$BH_PASS" = 1 ] && echo "  the network names $NAMED_HOST as its boothost and it did not answer;" \
                                     "asking again for up to ${BH_WAIT}s"
            sleep "${STORM_BOOTHOST_RETRY:-3}"
        done
        if [ -n "$BOOTHOST" ]; then
            [ "$BH_PASS" -gt 1 ] && echo "  $BOOTHOST answered after $(( $(date +%s) - BH_START ))s ($BH_PASS tries)"
            echo "Appliance: $BOOTHOST"
            # boot-local claims a fresh clone from it when this disk's
            # records name extents a cut-short flow-over never moved (#171).
            export STORMBLOCK_BOOTHOST="$BOOTHOST"
        else
            if [ -n "$NAMED_HOST" ]; then
                BOOTHOST_WHY="the boothost the network names ($NAMED_HOST) did not answer in $(( $(date +%s) - BH_START ))s ($BH_PASS tries)"
            else
                BOOTHOST_WHY="no appliance answered (tried:$CANDIDATE_HOSTS)"
            fi
            echo "No appliance: $BOOTHOST_WHY"
        fi
    fi
    # --- END appliance discovery

    # An explicit, one-shot wipe.
    #
    # There is no other way to clear a drive. `/dev` inside a container is
    # minimal — `/dev/sda` there is a regular empty file, so a shell on the
    # running node reads zero bytes from it and `blockdev` answers
    # "Inappropriate ioctl for device". The initramfs is the only place with
    # the real device.
    #
    # It has to be asked for by name, every time, and it is never a policy:
    # this clears the front and back of a disk, which is the partition table
    # and the first slab superblock. Nothing infers it and nothing retries it.
    if [ -n "${WIPE:-}" ] && [ -n "${WIPE_DONE:-}" ]; then
        : # a wipe runs once per boot, never per retry
    elif [ -n "${WIPE:-}" ] && [ -b "$WIPE" ]; then
        WIPE_DONE=1
        # Say which device, by size, before touching it. `/dev/sda` is not a
        # stable identity on this hardware — it has been the 2 TB disk and it
        # has been the iDRAC virtual floppy — and a wipe that reports success
        # against the wrong one is the worst outcome available here.
        echo "Wiping the partition table and slab headers on $WIPE (rd.stormblock.wipe)"
        echo "  device:  $(blockdev --getsize64 "$WIPE" 2>/dev/null || echo "size unknown") bytes"
        dd if=/dev/zero of="$WIPE" bs=1M count=8 conv=fsync 2>/dev/null \
            && echo "  front cleared"
        END=$(( $(blockdev --getsize64 "$WIPE" 2>/dev/null || echo 0) / 1048576 - 8 ))
        [ "$END" -gt 0 ] && dd if=/dev/zero of="$WIPE" bs=1M count=8 seek="$END" conv=fsync \
            2>/dev/null && echo "  back cleared (the mirror GPT)"
        # Read it back, because "dd exited 0" is not "the bytes are gone".
        #
        # The first wipe printed `front cleared` and `back cleared` and the
        # very next boot still found a GPT on the drive with a data slab in
        # partition 1 — read off the device itself, not out of the kernel's
        # cached table. One of those two statements was false and nothing in
        # the log said which. So: count the non-zero bytes in the sector the
        # GPT header lives in, and in the protective MBR before it.
        for probe in 0 1; do
            left=$(dd if="$WIPE" bs=512 skip="$probe" count=1 2>/dev/null \
                   | tr -d '\000' | wc -c)
            if [ "${left:-1}" -eq 0 ]; then
                echo "  LBA $probe: clear"
            else
                echo "  LBA $probe: $left byte(s) still set — THE WIPE DID NOT TAKE"
            fi
        done
        # Clearing the bytes is not clearing the partition table.
        #
        # The kernel read that table when it saw the disk, and it keeps it:
        # /dev/sda1 and /dev/sda2 stay, `blkid` still names them, and anything
        # that asks the *kernel* what is on this drive gets the answer from
        # before the wipe. So the wipe ran, reported "front cleared", and the
        # very next step still refused the drive:
        #
        #     refusing to format /dev/sda for flow-over: /dev/sda partition 1
        #     (stormblock-data) is typed as a stormblock data slab
        #
        # naming a partition that no longer existed on the disk it named.
        if blockdev --rereadpt "$WIPE" 2>/dev/null; then
            echo "  partition table re-read - the kernel now sees a blank drive"
        else
            # A drive with something open on it refuses the re-read, and then
            # the stale table is still live: say so rather than let the next
            # step fail describing partitions that are already gone.
            echo "  WARNING: the kernel would not re-read $WIPE's partition table."
            echo "           Its old partitions are still live; a reboot clears them."
        fi
    elif [ -n "${WIPE:-}" ]; then
        echo "rd.stormblock.wipe=$WIPE is not a block device - ignoring"
    fi

    # --- BEGIN boothost claim
    # Ask the appliance which image this machine boots, and claim it: sets
    # CLAIMED to the attach URI. A function because two places ask (#236): the
    # diskless path below, and the local-slab probe, which has to know what
    # this machine is assigned before it can tell an install from a reboot.
    # Returns non-zero, having said why, rather than dropping to a shell: the
    # probe's caller still has a local disk to boot.
    CLAIMED=""
    boothost_claim() {
        [ -n "$CLAIMED" ] && return 0
        # The identity is not worked out here. stormbootx named the machine
        # and claimed on it before Linux existed; this asks again in its own
        # right — the firmware's block device went with the UEFI that
        # published it — but on the *same* name, handed down rather than
        # rediscovered (the boot identity block, #249). Two implementations
        # of "who is this machine" drift, and they did: on a MicroCloud blade
        # the firmware's name is server8 and SMBIOS says the chassis serial.
        if [ "$BOOTTAG_FROM" = firmware ] && [ -r "${STORM_DMI:-/sys/class/dmi/id}/product_serial" ]; then
            _serial=$(tr -d " \n" < "${STORM_DMI:-/sys/class/dmi/id}/product_serial")
            [ -n "$_serial" ] && [ "$_serial" != "$BOOTTAG" ] \
                && echo "  SMBIOS serial $_serial is not used: the firmware claimed as $BOOTTAG"
        fi
        #
        # When nothing handed it down - a loader older than #249, or no
        # stormbootx at all - read it where the old firmware read it, and
        # say that it is a guess. SMBIOS type 1 serial is the Dell service
        # tag. It is not a machine's identity on a chassis that shares one.
        if [ -z "$BOOTTAG" ] && [ -r "${STORM_DMI:-/sys/class/dmi/id}/product_serial" ]; then
            BOOTTAG=$(tr -d " \n" < "${STORM_DMI:-/sys/class/dmi/id}/product_serial")
            # "Not Specified" and friends are what firmware writes when it has
            # nothing to say, and they are not a machine's identity: every VM
            # from one hypervisor would answer the same string and claim each
            # other's images.
            case "$BOOTTAG" in
            NotSpecified|Default*|None|Unknown|ToBeFilledByO.E.M.|"") BOOTTAG="" ;;
            esac
            [ -n "$BOOTTAG" ] && BOOTTAG_FROM=smbios \
                && echo "Service tag from SMBIOS: $BOOTTAG (a guess: the firmware handed no name down)"
        fi
        # No serial? Use the SMBIOS UUID (stormcos#46).
        #
        # A Dell has a service tag and a VM has none — Proxmox sets `uuid=`
        # and leaves `serial=` empty — so a machine that is not hardware could
        # not be told which image was its own at all. It dropped to a shell
        # saying SMBIOS had no tag, which is true and not useful.
        #
        # The UUID is the same *kind* of fact: one per machine rather than per
        # interface, stable across a NIC being replaced, and already set by
        # every hypervisor. A MAC was the other candidate and is worse on both
        # counts — a machine with two NICs has two identities, and replacing a
        # card changes who the machine is for no reason anyone would expect.
        #
        # Serial first, so a deliberately-assigned name wins over a generated
        # hex string: `boothost/flow-1` is readable and `boothost/28bae105-…`
        # is not, and on hardware the serial *is* the service tag, so nothing
        # about the Dell path changes.
        if [ -z "$BOOTTAG" ] && [ -r "${STORM_DMI:-/sys/class/dmi/id}/product_uuid" ]; then
            BOOTTAG=$(tr -d " \n" < "${STORM_DMI:-/sys/class/dmi/id}/product_uuid")
            [ -n "$BOOTTAG" ] && BOOTTAG_FROM=smbios \
                && echo "Machine UUID from SMBIOS: $BOOTTAG (a guess: the firmware handed no name down)"
        fi
        if identity_guessed; then
            echo "  a guessed name boots, and installs over no disk that carries a slab (#249)"
        fi
        [ -n "$BOOTTAG" ] && export STORMBLOCK_BOOT_TAG="$BOOTTAG"
        if [ -z "$BOOTTAG" ]; then
            echo "Nothing identifies this machine to $BOOTHOST:"
            echo "  not on the command line (rd.stormblock.tag=), no SMBIOS serial,"
            echo "  and no SMBIOS UUID. On a VM, set one:"
            echo "    qm set <id> --smbios1 serial=\$(printf %s <name> | base64),base64=1"
            return 1
        fi
        # The host NQN follows the tag the same way, in the format stormbootx
        # composes (src/main.rs): what the firmware presented on its connect
        # is what Linux presents on its own.
        if [ -z "$HOSTNQN" ]; then
            HOSTNQN="nqn.2026-09.lo.storm:host-$BOOTTAG"
            export STORMBLOCK_HOST_NQN="$HOSTNQN"
            echo "Host NQN: $HOSTNQN (from the machine's name)"
        fi
        echo "Asking $BOOTHOST which image $BOOTTAG boots..."
        CLAIMED=$("${STORM_STORMBLOCK:-/usr/sbin/stormblock}" boot-claim --boothost "$BOOTHOST" --tag "$BOOTTAG")
        if [ -z "$CLAIMED" ]; then
            echo "No image is assigned to $BOOTTAG on $BOOTHOST"
            return 1
        fi
        echo "  claimed: $CLAIMED"
    }
    # --- END boothost claim

    # --- BEGIN boot hook (covered by tests/initramfs-boot-hook.sh)
    #
    # Ask an installed hook where this node boots from, *before* probing the
    # device the command line names (#109).
    #
    # The probe below is deliberately narrow: it asks whether the one device
    # the cmdline names is a slab this node can boot. That is the right
    # question for the image the cmdline belongs to and the wrong one for a
    # machine, three ways, all of them seen on hardware:
    #
    #   - The cmdline is a pallet member and is identical on every machine
    #     that boots the image, so `rd.stormblock.slab=/dev/sda2` is a guess
    #     about enumeration order. The slab may be on another disk entirely.
    #   - A slab is not the same thing as a bootable disk. One formatted and
    #     never filled answers "2047 slots, 2047 free" and boots nothing.
    #   - Nothing in a slab superblock records an owner, so a disk moved
    #     between chassis is indistinguishable from one that was always here
    #     — and the hostname on it is the node CA's subject CN.
    #
    # A hook can answer all three, because it may look wherever it likes.
    # This takes a dependency on no particular one: anything executable in
    # /etc/stormblock/boot.d runs, in order, and /sbin/zeroboot is tried last
    # because that is the hook that exists today. Install nothing and this
    # loop does nothing — the probe below decides exactly as it always has.
    #
    #   exit 0   ZB_ACTION=boot-local, with ZB_SLAB (and ZB_SLAB_ID,
    #            ZB_VOLUME, ZB_DRIVE, which are reported)
    #   exit 2   ZB_ACTION=ask-appliance, with ZB_REASON
    #   exit 1   ZB_ACTION=error, with ZB_REASON
    #
    # A deciding hook may also name a drive this node could take, in
    # ZB_TAKEABLE — see the assimilation policy further down. It travels with
    # *either* decision, and in practice with `ask-appliance`: a node with
    # nothing of its own boots from the appliance and assimilates a blank
    # drive on the way, which is one boot, not two.
    #
    # Anything else — a hook that exits 0 and names no slab, or names one
    # that is not here — is treated as an error: the next hook runs, and with
    # none left the probe decides. A hook is asked, never obeyed blindly.
    #
    # **Nothing a hook prints is executed.** The contract is `KEY='value'`
    # lines because the consumer is busybox `sh` with no `jq`, and the
    # obvious reading of that is `eval "$(hook boot)"` — but this is PID 1,
    # and there `eval` makes a stray log line on stdout a command this shell
    # runs as root before there is a system to run it on. The four values
    # wanted are read out with `sed` instead. The worst a misbehaving hook
    # can do is be ignored.
    hook_value() { # key -> the last value the hook printed for it
        printf '%s\n' "$HOOK_OUT" \
            | sed -n "s/^$1=//p" \
            | tail -n 1 \
            | sed "s/^'//; s/'$//; s/'\\\\''/'/g"
    }
    HOOK_DECIDED=""
    HOOK_TAKEABLE=""
    for hook in "${STORM_BOOT_HOOK_DIR:-/etc/stormblock/boot.d}"/* \
                "${STORM_BOOT_HOOK_LEGACY:-/sbin/zeroboot}"; do
        [ -n "$HOOK_DECIDED" ] && break
        [ -f "$hook" ] && [ -x "$hook" ] || continue
        echo "Boot hook: $hook"
        # stdout is captured; stderr and /dev/kmsg are the hook's own voice
        # and go straight to the console, which is where its progress belongs.
        HOOK_OUT=$("$hook" boot)
        HOOK_RC=$?
        ZB_ACTION=$(hook_value ZB_ACTION)
        ZB_REASON=$(hook_value ZB_REASON)
        case "$HOOK_RC" in
        0)
            case "$ZB_ACTION" in
            ""|boot-local) ;;
            *)
                echo "  exited 0 but said '$ZB_ACTION' - ignoring it"
                continue
                ;;
            esac
            ZB_SLAB=$(hook_value ZB_SLAB)
            if [ -z "$ZB_SLAB" ]; then
                echo "  says boot-local and names no slab - ignoring it"
                continue
            fi
            case "$ZB_SLAB" in
            *://*) ;;
            *)
                if [ ! -e "$ZB_SLAB" ]; then
                    echo "  says boot from $ZB_SLAB, which is not on this machine - ignoring it"
                    continue
                fi
                ;;
            esac
            SLAB="$ZB_SLAB"
            HOOK_DECIDED="local"
            HOOK_TAKEABLE=$(hook_value ZB_TAKEABLE)
            ZB_SLAB_ID=$(hook_value ZB_SLAB_ID)
            ZB_DRIVE=$(hook_value ZB_DRIVE)
            ZB_VOLUME=$(hook_value ZB_VOLUME)
            echo "  boot local: $SLAB${ZB_DRIVE:+ (drive $ZB_DRIVE)}${ZB_SLAB_ID:+ slab $ZB_SLAB_ID}"
            # The cmdline wins when it named a volume: that is an operator
            # saying *which*, and the hook is answering *where*.
            if [ -n "$ZB_VOLUME" ] && [ -z "$VOLUME" ]; then
                VOLUME="$ZB_VOLUME"
                echo "  boot volume: $VOLUME (named by the hook)"
            fi
            ;;
        2)
            SLAB=""
            HOOK_DECIDED="appliance"
            HOOK_TAKEABLE=$(hook_value ZB_TAKEABLE)
            echo "  ask the appliance${ZB_REASON:+: $ZB_REASON}"
            ;;
        *)
            echo "  failed (exit $HOOK_RC)${ZB_REASON:+: $ZB_REASON} - carrying on without it"
            ;;
        esac
    done
    # --- END boot hook

    # One image, two lives, one command line.
    #
    # A node netboots once to install itself and then boots from the disk it
    # installed onto — with the same cmdline, because the cmdline is a pallet
    # member and there is only one of it. So the local slab is tried first and
    # the appliance is the fallback: on the bootstrap boot the disk holds no
    # slab and the node asks, and after the install it does, so the node stops
    # asking. Nothing has to be rewritten between the two.
    #
    # Existence is not the test. The R230 that found this has a 2 TB disk with
    # four partitions on it from a previous life, so /dev/sda is very much
    # there and is not a slab.
    # A hook that decided is not second-guessed here: it looked at more than
    # this can — the whole of /sys/block, the ESP, the loader entry, whose
    # disk it is — and a probe that can only re-ask the narrower question
    # would overrule a better answer with a worse one.
    # --- BEGIN local-slab probe (covered by tests/initramfs-boot-hook.sh)
    SB="${STORM_STORMBLOCK:-/usr/sbin/stormblock}"
    # A disk this node cannot boot: with an appliance, ask it instead; with
    # none, stop here and say why (#294) — handing it to `boot-local` ended
    # in an engine exit whose error scrolled off the screen.
    PROBE_STOP=""
    probe_fallback() { # what is wrong with the disk
        if [ -n "$BOOTHOST" ]; then
            echo "$1 - asking $BOOTHOST instead"
            SLAB=""
        else
            PROBE_STOP="$1"
        fi
    }
    # Run with or without an appliance (#294): without one, the checks are
    # what stops a disk that cannot boot this node before it is handed over.
    if [ -z "$HOOK_DECIDED" ] && [ -n "$SLAB" ]; then
        case "$SLAB" in
        *://*) ;;
        *)
            # Positive evidence, not absence of a known error. A device that
            # is a slab says so:
            #
            #   /dev/sdb: slab 7661cf8b-... (role=data, tier=hot, ...)
            #
            # Anything else — "not a slab", "cannot open", an empty removable
            # drive — means ask. The first version of this looked for the
            # words "not a slab" and was caught out immediately: on the R230
            # /dev/sda is sometimes the WD disk and sometimes the iDRAC
            # virtual floppy, and an empty floppy answers ENOMEDIUM rather
            # than "not a slab", so the probe passed and the boot died on
            # "No medium found". A device path is not a stable identity.
            if [ ! -e "$SLAB" ]; then
                probe_fallback "No $SLAB on this machine"
            elif ! $SB slab list "$SLAB" 2>/dev/null \
                 | grep -qE ": slab [0-9a-f-]{36}"; then
                probe_fallback "$SLAB is not a slab"
            else
                # A slab is not the same thing as a slab this node can boot.
                #
                # The test was "is there a slab here", and a *half* slab
                # passes it. This machine took its drive over, the flow-over
                # was still copying goldens onto it when it was rebooted, and
                # the next boot found the incomplete slab, believed it, and
                # came up with a login prompt and no services. Nothing said
                # what was wrong, because from the probe's point of view
                # nothing was.
                #
                # So the evidence has to be the thing actually needed: the
                # root volume, by the name the command line asks for. A slab
                # that cannot answer that is not this node's boot disk, and
                # the appliance is - which is the same fallback that already
                # covers a disk with no slab at all.
                #
                # `slab volumes` rather than `image inspect` (#108). Inspect
                # reads a *disk*: it wants a GPT, finds the slab partitions in
                # it and reports what each holds. Handed the partition itself
                # — which is what a loader entry names, `rd.stormblock.slab=
                # /dev/sda2` — it fails with "no usable GPT on this device",
                # and this branch read that as "no boot volume" and sent every
                # such node to the appliance. `slab volumes` asks the slab in
                # front of it, whether that is a partition, a whole disk or a
                # file.
                VOL="${VOLUME:-stormpump}"
                VOLS=$($SB slab volumes "$SLAB" 2>/dev/null)
                case "$VOLS" in
                *"keeps no volume metadata"*)
                    # The slab cannot answer, which is not the same as
                    # answering no. Old slabs keep their records beside them
                    # rather than on them, and `rd.stormblock.meta=` is the
                    # node saying where. With nowhere named there is nothing
                    # to check and nothing to trust, so the appliance decides.
                    if [ -n "$META" ]; then
                        echo "$SLAB keeps no volume metadata - trusting rd.stormblock.meta=$META"
                    else
                        probe_fallback "$SLAB keeps no volume metadata and no rd.stormblock.meta="
                    fi
                    ;;
                *)
                    # By name or by uuid, because `stormblock.volume=` takes
                    # either and the line carries both — the name after
                    # ": volume " and the uuid at the end.
                    if ! printf '%s\n' "$VOLS" | grep -qE ": volume $VOL | $VOL\$"; then
                        probe_fallback "$SLAB has no '$VOL' volume"
                    else
                        # The root is not the whole boot.
                        #
                        # A flow-over moves the *system* half — the goldens —
                        # and deliberately leaves the data half where it is,
                        # because migrating a slab that is being written
                        # corrupts it. So a drive part-way through is a drive
                        # that has `stormpump` and every golden on it and none
                        # of the writable volumes, and answering this probe on
                        # the root volume alone declared it bootable.
                        #
                        # It was not. The node attached its own disk, restored
                        # 75 volumes, dropped 5712 extent mappings that pointed
                        # into the appliance's slabs, and died on
                        #
                        #   Error: volume 'stormcert-data' not found in slab
                        #   metadata
                        #
                        # after listing the seventy-five it did have. Every
                        # volume the command line mounts has to be here, or
                        # this disk cannot boot this node yet.
                        # This disk's own release says what it mounts (#262).
                        mounts_from "$SLAB"
                        MISSING=""
                        for entry in $(printf '%s' "$MOUNTS" | tr ',' ' '); do
                            # Optional (#288): absent is not missing.
                            case "$entry" in \?*) continue ;; esac
                            name="${entry%%:*}"
                            [ -n "$name" ] || continue
                            printf '%s\n' "$VOLS" \
                                | grep -qE ": volume $name | $name\$" && continue
                            MISSING="$MISSING $name"
                        done
                        if [ -n "$MISSING" ]; then
                            set -- $MISSING
                            PROBE_MISSING="$MISSING"
                            echo "  ${MOUNTS_FROM:+the mount list ($MOUNTS_FROM) names }$# volume(s) $SLAB does not have:$(printf '%s' "$MISSING" | cut -c1-120)"
                            probe_fallback "$SLAB has '$VOL' but is missing $# mounted volume(s)"
                        fi
                    fi
                    ;;
                esac
            fi
            ;;
        esac
    fi
    # A local disk that can boot is not yet a reason to boot it (#236).
    #
    # stormbootx claims on every boot until the appliance serves boot intents
    # (forge on stormblock < 20, #235), so the network image this kernel came
    # from says nothing about whether the machine is being reinstalled or
    # just rebooted - and the probe above booted the old disk either way. A
    # new release never went on: the old one came back up under the new
    # kernel, or, once the disk had been wiped by hand, the new goldens came
    # up on the old data half (11.53 on 11.51's fastetcd, stormcentral#196).
    #
    # What tells the two apart is the release. Ask the appliance what this
    # machine is assigned and whether the local disk already holds it: held
    # is a reboot and boots the disk as before; not held is an install, which
    # boots the claimed image and lays a fresh slab over this disk (the
    # survey below). An install the appliance asked for (the ticket, #148)
    # installs whatever the disk holds. No answer, or one that cannot say,
    # boots the disk: the network this kernel came over is not a reason to
    # drop a node that can boot to a shell.
    INSTALL_OVER=""
    if [ -z "$HOOK_DECIDED" ] && [ -n "$SLAB" ] && [ -n "$BOOTHOST" ] \
       && [ "${ASSIMILATE:-}" != off ]; then
        case "$SLAB" in
        *://*) ;;
        *)
            echo "$SLAB can boot this node; asking $BOOTHOST whether it holds the release assigned here"
            if boothost_claim; then
                if [ -e "${STORM_INSTALL_TICKET:-/run/stormblock/install.json}" ]; then
                    echo "  $BOOTHOST asks for an install: booting the claimed image, installing over $SLAB"
                    HELD_RC=1
                else
                    HELD=$($SB slab holds "$SLAB" "$CLAIMED" 2>&1)
                    HELD_RC=$?
                    echo "  $HELD"
                fi
                # A guessed name's image is not a reason to replace this disk:
                # it may be another machine's (#249). The disk boots.
                if [ "$HELD_RC" = 1 ] && identity_guessed; then
                    echo "  NOT INSTALLING: $BOOTTAG is a guess from SMBIOS, and its image may be"
                    echo "  another machine's - booting $SLAB as before (rd.stormblock.trust-smbios=1 to allow)"
                    HELD_RC=guess
                fi
                case "$HELD_RC" in
                0) echo "  the same release: booting $SLAB, its data kept" ;;
                3)
                    # The same release, its flow-over cut short (a power cut
                    # during the install, #258): the disk boots and the
                    # engine finishes the flow-over from a fresh clone (#171).
                    # Installing over it lost every object on 11.63.
                    echo "  the same release, its install cut short: booting $SLAB, its data kept;"
                    echo "  the engine finishes the flow-over from the clone just claimed as $BOOTTAG"
                    # That clone, not a second claim: the one `slab holds`
                    # just compared is the one known to be this release (#259).
                    export STORMBLOCK_RESUME_SOURCE="$CLAIMED"
                    ;;
                guess) ;;
                1)
                    echo "  INSTALL: a release $SLAB does not hold - booting the claimed image;"
                    echo "  the system half of its disk is laid again, its data half kept (#311)"
                    # The disk, not the partition the cmdline named.
                    case "$SLAB" in
                    /dev/nvme*p[0-9]*) INSTALL_OVER="${SLAB%p[0-9]*}" ;;
                    /dev/sd*[0-9])     INSTALL_OVER="${SLAB%%[0-9]*}" ;;
                    *)                 INSTALL_OVER="$SLAB" ;;
                    esac
                    SLAB="$CLAIMED"
                    ;;
                *) echo "  cannot tell which release $SLAB holds - booting it as before" ;;
                esac
            else
                echo "  no image from $BOOTHOST - booting $SLAB as before"
            fi
            ;;
        esac
    fi
    # Never skipped without a word (#294): with no appliance there is nobody
    # to say which release this machine is assigned, so a disk that holds an
    # older one boots as it is.
    if [ -z "$HOOK_DECIDED" ] && [ -n "$SLAB" ] && [ -z "$BOOTHOST" ] && [ -z "$PROBE_STOP" ] \
       && [ "${ASSIMILATE:-}" != off ]; then
        case "$SLAB" in
        *://*) ;;
        *)
            echo "RELEASE CHECK SKIPPED: ${BOOTHOST_WHY:-no appliance}."
            echo "  Booting $SLAB as it is, without asking which release is assigned to this machine:"
            echo "  if this boot was meant to install another release, it has not."
            ;;
        esac
    fi
    # --- END local-slab probe

    # The disk cannot boot this node and there is no appliance to boot from
    # instead (#294): say so, with what the disk holds, rather than handing
    # it to `boot-local` to fail on the first missing volume.
    if [ -n "$PROBE_STOP" ]; then
        echo "FATAL: $PROBE_STOP."
        echo "  ${SLAB} holds: $(disk_release "$SLAB")"
        [ -n "${PROBE_MISSING:-}" ] && echo "  missing:$PROBE_MISSING"
        echo "  No appliance to boot from instead: ${BOOTHOST_WHY:-none was found}."
        echo "  This boot may have been meant to install another release; nothing was installed."
        rescue_shell
    fi

    # Everything from here to the engine's start runs again when a local
    # disk's root does not come up and the boot falls back to the claimed
    # image (#244, `root_fallback` below): the survey decides the disk
    # afresh, and the mounts are numbered for the image that boots.
    launch_local() {
    # Diskless: this machine's slab is a namespace on the appliance, and which
    # one is a per-machine fact the baked-in cmdline cannot carry. Ask, keyed
    # on the service tag. The firmware made the same claim a stage earlier to
    # load the kernel, but what it published was a UEFI block device and that
    # ceased to exist when the kernel started, so the node asks in its own
    # right rather than inheriting anything.
    if [ -z "$SLAB" ] && [ -n "$BOOTHOST" ]; then
        if ! boothost_claim; then
            echo "FATAL: $BOOTHOST gave this machine no image to boot"
            rescue_shell
        fi
        SLAB="$CLAIMED"
        echo "  slab: $SLAB"
    fi

    # A fabric URI is not a path and will never appear as one. stormblock
    # opens `nvme-tcp://` wherever it opens a device path, so there is nothing
    # to wait for — waiting is what turned a diskless boot into 30 seconds and
    # an initramfs shell.
    case "$SLAB" in
    *://*)
        echo "Slab is remote: $SLAB"
        ;;
    *)
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
            rescue_shell
        fi
        ;;
    esac

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
    # The release this boot runs says what it mounts (#262): read again, since
    # the slab may have changed since the probe (a claimed image, an install).
    mounts_from "$SLAB"
    mounts_optional "$SLAB"
    if [ -n "$MOUNTS" ]; then
        echo "Mount list: $(printf '%s' "$MOUNTS" | tr ',' '\n' | grep -c .) volume(s), from $MOUNTS_FROM"
    else
        echo "Mount list: none (no rd.stormblock.mount=, and no /etc/stormblock/mounts in ${VOLUME:-stormpump})"
    fi
    MOUNT_MAP=""
    if [ -n "$MOUNTS" ]; then
        OIFS=$IFS; IFS=,
        for entry in $MOUNTS; do
            IFS=$OIFS
            mname="${entry%%:*}"
            mmnt="${entry#*:}"
            if [ -z "$mname" ] || [ "$mname" = "$entry" ]; then
                echo "  WARNING: rd.stormblock.mount entry '$entry' has no :<path> - ignored"
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

    # Is there a local drive worth putting this node's writes on?
    #
    # A netbooted node's writable volumes are clones served from the
    # appliance: writable, and not durable, because a fresh clone is minted
    # every boot. `stormcos-state` makes it concrete — PID 1 reads the
    # hostname out of it and that hostname is the node CA's subject CN, so a
    # node whose state is remote cannot keep its own identity across a
    # reboot. And twenty nodes writing their logs across the network land on
    # one appliance.
    #
    # Looking comes before taking. A drive is only a candidate if it says
    # what it is: `stormblock slab list` prints `: slab <uuid>` for a slab and
    # names the role, and anything else - a foreign partition table, an empty
    # removable bay answering ENOMEDIUM - is not evidence of emptiness. The
    # policy decides what to do with that:
    #
    #   any    (default) take any drive that is not already a stormblock slab
    #   blank  take a drive that carries no slab and no partition table
    #   off    never take a drive
    #   force  take it even when it is one of ours, destroying the identity
    #
    # **The default is to take one, because this image is an installer.** It
    # was `off`, which made the common case - a machine with one drive, booted
    # from the network to be installed - do nothing and keep every write on the
    # appliance, until somebody knew to add a cmdline parameter. Booting this
    # image *is* the decision: nobody netboots an installer at a machine whose
    # disk they mean to keep, and a node that must not touch its drive says
    # `off`.
    #
    # And a drive with somebody else's ext4 or a previous life's partition
    # table is not a reason to stop. Garbage cannot be interpreted safely -
    # a stale backup GPT, an LVM label, an mdraid superblock at the end of the
    # device are each read by something that scans rather than asks - so
    # `lay_node_slabs` destroys the ends of the drive before it lays the
    # table. The alternative to installing over it is a setup API and a remote
    # UI to drive it, which is a great deal of machinery to decide something
    # the boot already decided.
    #
    # `any` still refuses a drive that carries one of *our* slabs. That is not
    # caution about garbage, it is the node's identity: the data partition
    # holds the CA key and the ServiceAccount signing key, and nothing can
    # mint those again. `force` is the deliberate act for a drive whose
    # identity is spent.
    #
    # `any` is a fleet-wide statement that local drives are ours to use, not a
    # fact about one machine. A drive that already carries a *data* slab is
    # refused by boot-local itself, whatever the policy says, because that
    # partition holds this node's CA key and nothing can mint it again.
    #
    # `force` is the exception, and it is a different kind of statement: not
    # "local drives are ours" but "this drive is spent". It is what recovers a
    # machine whose drive carries an install that was abandoned half-written,
    # which is otherwise refused by the survey and by boot-local both, on
    # every boot, with no way out — the guard cannot tell a dead identity from
    # a live one, so it protects both.
    # --- BEGIN assimilate survey (covered by tests/initramfs-boot-hook.sh)
    # An install the appliance asked for (#148): `boot-claim` leaves a ticket
    # when the claim answered `intent: install`. That is the operator saying
    # "this drive is spent", so it is `force` - except that an explicit
    # `rd.stormblock.assimilate=off` on this machine still means no.
    #
    # Neither install, and no drive that carries a slab, when this machine's
    # name is a guess (#249): the image it claimed, and the intent the
    # appliance stated, may be another machine's. A blank drive is still
    # taken - nothing on it is lost, and the next boot under the right name
    # finds the release not held and installs it.
    GUESSED=""
    if identity_guessed 2>/dev/null; then
        GUESSED=1
    fi
    if [ -n "$GUESSED" ] && [ "${ASSIMILATE:-}" != off ]; then
        echo "  $BOOTTAG is a guess from SMBIOS: only a blank drive is taken (#249)"
        ASSIMILATE=blank
    elif [ -e "${STORM_INSTALL_TICKET:-/run/stormblock/install.json}" ]; then
        if [ "${ASSIMILATE:-}" = off ]; then
            echo "  an install was requested, and rd.stormblock.assimilate=off says no"
        else
            ASSIMILATE=force
            echo "  an install was requested: the local disk is taken whatever it carries"
        fi
    fi
    # An install lays the system half again and keeps the data half; the same
    # release is a recovery (#311, owner 2026-10-06: "How can you wipe a
    # production node data?" - superseding #261's install = wipe).
    #
    # This boot runs from the image it claimed, and a local drive may carry
    # this node's data slab. Which release that drive holds decides, with or
    # without a boot intent from the appliance:
    #
    #   another release (`slab holds` exit 1), the probe ruled the disk an
    #       install (`INSTALL_OVER`), or the appliance asked for an install:
    #       an INSTALL. With a data slab on the disk, never `force`d:
    #       `boot-local` lays the system half again and adopts the data half,
    #       and the release's /etc/stormblock/data-volumes says what happens to
    #       each data volume it names (keep, replace, migrate; #122). A
    #       release that adds a data volume brings it (stormcos#236's
    #       kubelet-data). With no data slab, the drive is laid fresh.
    #   the same release (0), or the same release cut short (3, a power cut
    #       during the install's flow-over, #258/#259): RECOVERY. The disk is
    #       kept, its data half untouched.
    #   cannot say (2): neither. Every local drive is left alone this boot
    #       and it runs from the appliance.
    #
    # Updating a running node to a new release is stormupdate's (stormupdate#1):
    # it stages the new volumes and reboots, and that boot is a same-release
    # boot.
    #
    # With no data slab anywhere and no intent stated (#236), a drive is laid
    # fresh (forced) as well: there is nothing to keep, and a partition table
    # from an abandoned install must not stop it. An intent of `install` (the
    # ticket) lays fresh only a drive with no data slab; `off` on this machine
    # still means no.
    SURVEY_SB="${STORM_STORMBLOCK:-/usr/sbin/stormblock}"
    SURVEY_SYS="${STORM_SYS_BLOCK:-/sys/block}"
    SURVEY_DEV="${STORM_DEV:-/dev}"
    # Which drives an install may take (#273: a NetApp shelf on the Dell, for
    # stormraid). Three rules, in order:
    #
    #   1. The drive rd.stormblock.slab= names, when it is on this machine,
    #      is the only one. Every other drive is left alone and said so.
    #   2. A drive behind a SAS expander or in an SES enclosure - a disk
    #      shelf - is never taken by the scan (rd.stormblock.allow-external=1
    #      for a server whose own bays sit behind one).
    #   3. "Nobody's" means blank: its first and last MiB are zeros. Anything
    #      else - a partition table, a stormraid member, md, LVM, ZFS, a
    #      filesystem - is somebody's, and left.
    named_disk() { # -> the named slab's disk, as a /sys/block name
        case "${SLAB_NAMED:-}" in
        /dev/nvme*p[0-9]*) _nd="${SLAB_NAMED%p[0-9]*}" ;;
        /dev/sd*[0-9])     _nd="${SLAB_NAMED%%[0-9]*}" ;;
        /dev/*)            _nd="$SLAB_NAMED" ;;
        *)                 return 0 ;;
        esac
        _nd="${_nd##*/}"
        [ -e "$SURVEY_SYS/$_nd" ] && echo "$_nd"
        return 0
    }
    NAMED_DISK=$(named_disk)
    drive_external() { # /sys/block/X -> 0 when the drive is in a shelf
        [ "${ALLOW_EXTERNAL:-}" = 1 ] && return 1
        case "$(readlink -f "$1" 2>/dev/null)" in */expander-*) return 0 ;; esac
        for _e in "$1"/device/enclosure_device:*; do
            [ -e "$_e" ] && return 0
        done
        return 1
    }
    drive_signature() { # dev /sys/block/X -> what it carries; nothing when blank
        _got=$(dd if="$1" bs=1048576 count=1 2>/dev/null | wc -c)
        if [ "${_got:-0}" -eq 0 ]; then
            echo "nothing that can be read"
            return 0
        fi
        case "$(dd if="$1" bs=8 count=1 2>/dev/null)" in
        STORMRD1*) echo "a stormraid superblock"; return 0 ;;
        esac
        if [ "$(dd if="$1" bs=1048576 count=1 2>/dev/null | tr -d '\000' | wc -c)" -ne 0 ]; then
            echo "data in its first MiB"
            return 0
        fi
        _sec=$(cat "$2/size" 2>/dev/null || echo 0)
        if [ "$_sec" -ge 4096 ] && [ "$(dd if="$1" bs=1048576 skip=$((_sec / 2048 - 1)) count=1 2>/dev/null \
                | tr -d '\000' | wc -c)" -ne 0 ]; then
            echo "data in its last MiB"
        fi
        return 0
    }
    may_take() { # /sys/block/X dev -> 0 when rules 1 and 2 allow the drive
        if [ -n "$NAMED_DISK" ] && [ "${1##*/}" != "$NAMED_DISK" ]; then
            echo "  $2 is not the drive rd.stormblock.slab= names (/dev/$NAMED_DISK) - leaving it (#273)"
            return 1
        fi
        if drive_external "$1"; then
            echo "  $2 is in an external enclosure (a disk shelf) - leaving it (#273;"
            echo "  rd.stormblock.allow-external=1 if this machine's own bays are)"
            return 1
        fi
        return 0
    }
    local_data_slab() { # -> the first local drive that carries a data slab
        for d in "$SURVEY_SYS"/sd? "$SURVEY_SYS"/nvme?n?; do
            [ -e "$d" ] || continue
            [ "$(cat "$d/removable" 2>/dev/null)" = "1" ] && continue
            # The named drive, or (none named) an internal one (#273).
            if [ -n "$NAMED_DISK" ]; then
                [ "${d##*/}" = "$NAMED_DISK" ] || continue
            else
                drive_external "$d" && continue
            fi
            if "$SURVEY_SB" slab list "/dev/$(basename "$d")" 2>/dev/null | grep -q "role=data"; then
                echo "/dev/$(basename "$d")"
                return 0
            fi
        done
        return 1
    }
    release_on() { # disk -> sets RELEASE_ON: another | same | cut | unknown
        R_OUT=$("$SURVEY_SB" slab holds "$1" "$CLAIMED" 2>&1)
        R_RC=$?
        [ -n "$R_OUT" ] && echo "  $R_OUT"
        case "$R_RC" in
        1) RELEASE_ON=another ;;
        0) RELEASE_ON=same ;;
        3) RELEASE_ON=cut ;;
        *) RELEASE_ON=unknown ;;
        esac
    }
    # An install never wipes data (#311, owner 2026-10-06): it touches the
    # system drive only, and only its system half. A disk that carries a data
    # slab is installed over without `force`: `boot-local` lays the system half
    # again and adopts the data half, the release's keep/replace/migrate policy
    # deciding each data volume it names. `force` is left for a drive with no
    # data slab on it.
    has_data_slab() { # disk -> 0 when it carries a stormblock data slab
        "$SURVEY_SB" slab list "$1" 2>/dev/null | grep -q "role=data"
    }
    INSTALL_KEEP=""
    install_keeping() { # disk why
        INSTALL_OVER="$1"
        INSTALL_KEEP=1
        INSTALL_FRESH=1
        ASSIMILATE=any
        echo "  INSTALL: $1 $2 - laying its system half again for this"
        echo "  release; its data half is kept, every volume on it adopted (#311)"
    }
    INSTALL_FRESH=""
    KEPT=""
    RELEASE_ON=""
    if [ -n "$GUESSED" ]; then
        :
    elif [ -e "${STORM_INSTALL_TICKET:-/run/stormblock/install.json}" ] \
       && [ "${ASSIMILATE:-}" = force ]; then
        if [ -n "${INSTALL_OVER:-}" ] && has_data_slab "$INSTALL_OVER"; then
            install_keeping "$INSTALL_OVER" "is to be installed (asked by the appliance)"
        elif KEPT=$(local_data_slab); then
            install_keeping "$KEPT" "is to be installed (asked by the appliance)"
        else
            INSTALL_FRESH=1
        fi
    elif [ -n "${CLAIMED:-}" ] && [ "$SLAB" = "$CLAIMED" ]; then
        if [ "${ASSIMILATE:-}" = off ]; then
            echo "  booting the claimed image, and rd.stormblock.assimilate=off: no local drive is touched"
        elif [ -n "${INSTALL_OVER:-}" ] && has_data_slab "$INSTALL_OVER"; then
            install_keeping "$INSTALL_OVER" "does not hold the claimed release"
        elif [ -n "${INSTALL_OVER:-}" ]; then
            ASSIMILATE=force
            INSTALL_FRESH=1
            echo "  INSTALL: $INSTALL_OVER does not hold the claimed release and carries no data"
            echo "  slab - laying the release fresh on it"
        elif KEPT=$(local_data_slab); then
            release_on "$KEPT"
            case "$RELEASE_ON" in
            another)
                install_keeping "$KEPT" "holds another release than the one claimed"
                ;;
            same)
                echo "  RECOVERY: $KEPT holds the release claimed - kept, its data half untouched (#261)"
                ;;
            cut)
                echo "  RECOVERY: $KEPT holds the release claimed, its install cut short -"
                echo "  kept, its data half untouched (#258)"
                ;;
            *)
                ASSIMILATE=held
                echo "  LEFT ALONE: cannot tell which release $KEPT holds - neither wiped nor"
                echo "  merged; every local drive is left alone and this boot runs from the appliance (#261)"
                ;;
            esac
        elif [ -e "${STORM_NO_INTENT:-/run/stormblock/no-intent}" ]; then
            ASSIMILATE=force
            INSTALL_FRESH=1
            echo "  INSTALL: no local drive carries a data slab, so a fresh slab is laid (#236)"
        fi
    fi
    LOCAL_DISK=""
    # The disk the probe found bootable and ruled an install over: that is
    # the one to install onto, not whichever drive the scan meets first.
    if [ -n "$INSTALL_FRESH" ] && [ -n "${INSTALL_OVER:-}" ] && [ -e "$INSTALL_OVER" ]; then
        LOCAL_DISK="$INSTALL_OVER"
        echo "  installing over $INSTALL_OVER, the disk this machine booted from until now"
    fi
    if [ -n "$INSTALL_KEEP" ] && [ -z "$LOCAL_DISK" ]; then
        echo "  $INSTALL_OVER is not here to install over - every local drive is left alone"
        ASSIMILATE=held
    fi
    case "${ASSIMILATE:-any}" in
    off) echo "  rd.stormblock.assimilate=off: leaving every local drive alone" ;;
    held) echo "  leaving every local drive alone this boot; writes stay on the appliance (#261)" ;;
    blank|any|force)
        for d in "$SURVEY_SYS"/sd? "$SURVEY_SYS"/nvme?n?; do
            [ -n "$LOCAL_DISK" ] && break
            [ -e "$d" ] || continue
            dev="/dev/$(basename "$d")"
            [ "$(cat "$d/removable" 2>/dev/null)" = "1" ] && continue
            [ "$(cat "$d/size" 2>/dev/null || echo 0)" -gt 0 ] || continue
            # Do not eat the disk this boot is running from.
            case "$SLAB" in *"$(basename "$d")"*) continue ;; esac
            may_take "$d" "$dev" || continue
            probe=$("$SURVEY_SB" slab list "$dev" 2>&1)
            case "$probe" in
                *": slab "*|*"data slab"*)
                    # `force` is the one policy that answers "and take it
                    # anyway". A drive carrying a slab from an install that
                    # was abandoned is indistinguishable from one carrying a
                    # live node's identity, and under every other policy that
                    # drive is refused for the life of the machine — here by
                    # the survey, and again by `boot-local`'s own guard.
                    if [ "$ASSIMILATE" = force ]; then
                        LOCAL_DISK="$dev"
                        echo "  $dev already carries a stormblock slab - taking it anyway (force)"
                        break
                    fi
                    # A drive that is *this node's own layout* — both halves,
                    # data and system — is taken and updated rather than left
                    # alone. That is what an install is: `boot-local` replaces
                    # the system half, where the goldens live, and keeps the
                    # data half, which holds the CA key and the ServiceAccount
                    # signing key. Nothing irreplaceable is destroyed, so this
                    # needs no `force`.
                    #
                    # Leaving it alone is what made a reinstall a dead end: the
                    # node booted the fresh image, refused its own disk, and
                    # ran from the appliance for the rest of its life.
                    #
                    # Both halves are required. A lone data slab is an
                    # abandoned install and only `force` answers for it.
                    if [ "$ASSIMILATE" != blank ] \
                       && printf '%s\n' "$probe" | grep -q "role=data" \
                       && printf '%s\n' "$probe" | grep -q "role=system"; then
                        LOCAL_DISK="$dev"
                        echo "  $dev is this node's own layout - taking it to replace the system half"
                        break
                    fi
                    # **No data slab, no identity.** A drive whose slabs are
                    # all system-role holds goldens at most. Those are what
                    # every install replaces, and nothing on such a drive is
                    # this node's CA key or its ServiceAccount signing key,
                    # because those live only in a data slab. `boot-local`'s
                    # own guard (`data_slab_on`) already takes this view and
                    # lays fresh slabs over it. Only the survey refused.
                    #
                    # It refused on every boot. A 2 TB drive left by an older
                    # flow-over, one system slab across the whole disk with
                    # nothing in it, was "somebody else's arrangement". So the
                    # node claimed a fresh clone from the appliance every boot
                    # and nothing written to a -data volume survived a reboot
                    # (stormblock#118):
                    #
                    #   /dev/sda is already a stormblock slab - leaving it
                    #   no local drive to take; writes stay on the appliance
                    #
                    # `blank` still leaves it: that policy asks for a drive
                    # that carries nothing at all.
                    if [ "$ASSIMILATE" != blank ] \
                       && ! printf '%s\n' "$probe" | grep -q "role=data"; then
                        LOCAL_DISK="$dev"
                        echo "  $dev carries system slabs only - no identity on it; this node will take it"
                        break
                    fi
                    echo "  $dev is already a stormblock slab - leaving it" ;;
                *)
                    # Not a stormblock slab: taken only when blank (#273).
                    # `force` may still clear the drive the command line
                    # names, whatever it carries; never another.
                    sig=$(drive_signature "$SURVEY_DEV/${d##*/}" "$d")
                    if [ -n "$sig" ] && ! { [ "$ASSIMILATE" = force ] && [ "${d##*/}" = "$NAMED_DISK" ]; }; then
                        echo "  $dev carries $sig - not ours and not blank, leaving it (#273)"
                    elif [ "$ASSIMILATE" = blank ] && \
                       "$SURVEY_SB" slab list "$dev" 2>&1 | grep -q "partition"; then
                        echo "  $dev carries partitions and the policy is 'blank' - leaving it"
                    else
                        LOCAL_DISK="$dev"
                        echo "  $dev is nobody's - this node will take it"
                        break
                    fi ;;
            esac
        done
        [ -z "$LOCAL_DISK" ] && echo "  no local drive to take; writes stay on the appliance"
        # The override travels with the policy, not with the branch that
        # picked the drive.
        #
        # `slab list` and `boot-local` do not ask the same question. This
        # drive answered "not a slab" here — no whole-device slab magic — and
        # was chosen as nobody's, and then boot-local read its GPT, found a
        # partition typed as a data slab, and refused it. Setting the flag
        # only where the survey *saw* a slab left the common case, a drive
        # with a partition table from an abandoned install, still stuck.
        # Never over a data slab (#311): whatever path chose this drive, a
        # node's data half is not destroyed by an install. Without force a
        # drive with both halves has its system half laid again; one with a
        # data slab alone is refused by `boot-local`, and the node runs from
        # the appliance with the drive untouched.
        if [ "$ASSIMILATE" = force ] && [ -n "$LOCAL_DISK" ] && has_data_slab "$LOCAL_DISK"; then
            echo "  $LOCAL_DISK carries a data slab - not destroying it: an install"
            echo "  never wipes data (#311); its system half is laid again, the data kept"
            ASSIMILATE=any
        fi
        if [ "$ASSIMILATE" = force ] && [ -n "$LOCAL_DISK" ]; then
            FORCE_LOCAL=1
            echo "  policy is 'force': whatever $LOCAL_DISK carries will be destroyed"
            if [ -n "$INSTALL_FRESH" ]; then
                echo "  INSTALL: the old slab on $LOCAL_DISK is discarded - nothing of the"
                echo "  previous install is kept; a fresh slab is laid for this release"
            fi
        fi
        ;;
    *) echo "  unknown rd.stormblock.assimilate='$ASSIMILATE' (off|blank|any|force)" ;;
    esac
    # --- END assimilate survey

    # --- BEGIN hook takeable (covered by tests/initramfs-boot-hook.sh)
    # A hook may name the drive instead (#109).
    #
    # The policies above are a *fleet* statement — "local drives are ours" —
    # applied by a scan that can only ask `slab list` whether a drive is one
    # of ours. That is the right question for a policy and a weak one for a
    # drive: a foreign ext4, or the four partitions from a previous life that
    # a second-hand R230 actually carries, answers "not a slab" and is taken.
    # An operator typing `--local-disk /dev/sda` has looked at the drive,
    # which is the premise that makes the policy safe; the moment the path is
    # chosen by something other than a person, that premise is gone.
    #
    # A hook closes that: `ZB_TAKEABLE` is offered only for a drive it judged
    # blank — no table, no filesystem signature, no slab, nothing over the
    # network, nothing removable, and read back zero. That is strictly
    # stronger than "carries no data slab", so nothing offered here can trip
    # `boot-local`'s own guard, and the guard stays the last word.
    #
    # Precedence, most specific first:
    #
    #   assimilate=off      an operator saying no, and it means no
    #   a policy *named on the cmdline*, and the drive its scan chose
    #   the hook's offer    which beats the default scan, because the default
    #                       is not an instruction and the hook looked harder:
    #                       the scan can only ask `slab list` whether a drive
    #                       is ours, and the hook read the drive
    #
    # Never with `--local-disk-force`: force destroys whatever a drive
    # carries, and a drive that had to be forced is by definition not the
    # blank one a hook offered.
    if [ -n "$HOOK_TAKEABLE" ]; then
        # Offering the drive this boot is reading from would hand the node its
        # own root to reformat. zeroboot does not, and this is not the place to
        # find out that something else does. Matched the way the scan above
        # matches, so `/dev/sda` is recognised as where `/dev/sda2` came from.
        takeable_is_root=""
        case "$SLAB" in
        *"$(basename "$HOOK_TAKEABLE")"*) takeable_is_root=1 ;;
        esac
        if [ "${ASSIMILATE:-}" = off ]; then
            echo "  hook offers $HOOK_TAKEABLE, and rd.stormblock.assimilate=off says no"
        elif [ -n "$LOCAL_DISK" ] && [ -n "${ASSIMILATE:-}" ]; then
            echo "  hook offers $HOOK_TAKEABLE; keeping $LOCAL_DISK, which rd.stormblock.assimilate=$ASSIMILATE chose"
        elif [ ! -b "$HOOK_TAKEABLE" ] && [ ! -f "$HOOK_TAKEABLE" ]; then
            echo "  hook offers $HOOK_TAKEABLE, which is not on this machine - ignoring it"
        elif [ -n "$takeable_is_root" ]; then
            echo "  hook offers $HOOK_TAKEABLE, which is where this boot is coming from - ignoring it"
        else
            LOCAL_DISK="$HOOK_TAKEABLE"
            echo "  hook offers $LOCAL_DISK to assimilate onto - taking it"
        fi
    fi
    # --- END hook takeable

    # Attach the existing slab (no reformat), export boot volume as ublkb0.
    # Volume comes from --volume if given, else /etc/stormblock/boot.toml.
    # shellcheck disable=SC2086
    #
    # Its output goes to a log and is followed onto the console (#294): when
    # it exits, the root wait below repeats the end of it, which had scrolled
    # off the screen. The engine writing a file also cannot meet a broken
    # pipe, which its own messages panic on.
    mkdir -p /run/stormblock
    : > "$ENGINE_LOG"
    /usr/sbin/stormblock boot-local \
        --slab "$SLAB" \
        ${LOCAL_DISK:+--local-disk "$LOCAL_DISK"} \
        ${LOCAL_DISK:+${FORCE_LOCAL:+--local-disk-force}} \
        ${META:+--meta "$META"} \
        ${IMAGE_STORE:+--image-store "$IMAGE_STORE"} \
        ${VOLUME:+--volume "$VOLUME"} \
        $WR_ARGS >> "$ENGINE_LOG" 2>&1 &
    ROOTDEV=/dev/ublkb0
    }
    # The disk this boot's root comes from, when it is a local one: what a
    # root that does not come up falls back from (#244).
    LOCAL_ROOT_SLAB=""
    case "$SLAB" in
    ""|*://*) ;;
    *) [ -z "$HOOK_DECIDED" ] && LOCAL_ROOT_SLAB="$SLAB" ;;
    esac
    launch_local
else
    mkdir -p /run/stormblock
    : > "$ENGINE_LOG"
    /usr/sbin/stormblock boot-iscsi \
        --portal "$PORTAL" --port "$PORT" \
        --iqn "$IQN" --layout "$LAYOUT" --ublk >> "$ENGINE_LOG" 2>&1 &
    ROOTDEV=/dev/ublkb2   # partition index 2 = root
fi
STORMBLOCK_PID=$!
# The engine's output on the console as it comes (and kept in the log).
tail -n +1 -f "$ENGINE_LOG" 2>/dev/null &
FOLLOW_PID=$!

# Long enough for a first boot that is also doing work.
#
# Thirty seconds was the whole budget, and it was set when the only thing
# between here and the root device was opening a slab. A node taking its local
# disk now seeds the writable half *before* exporting anything — deliberately,
# because that is the one moment when nothing is mounted and not a byte has
# been written — and on this hardware that took 28.3 seconds for 3041 extents.
# The boot gave up 1.7 seconds before the work it was waiting for landed:
#
#   Flow-over: seeding the data half of /dev/sda - 32 volume(s), 3041 extent(s)
#   FATAL: root device /dev/ublkb0 not found after 30s
#   Flow-over: data half seeded - 3041 extent(s) onto /dev/sda in 28.3s
#
# A copy proportional to what a node stores cannot share a deadline with
# "opening a device took too long". The failure this guards against — an
# engine that died, a slab that will not open — is not more likely at five
# minutes than at thirty seconds, it just takes longer to say so, and it says
# so on a machine that would otherwise sit at a prompt forever.
#
# **And a deadline is not the signal; the engine is.** 300 s gave up on a
# boot whose engine was alive and one minute from done — seeding took 382.6 s
# on the R230, PID 1 dropped to a shell, and every device then came up behind
# it with nothing left to use them (#118). An engine that is still running is
# working; one that has exited has failed, and that is known at once. So the
# wait ends when the root appears or the engine dies, and the deadline only
# bounds an engine that is alive and stuck — generously, since a first boot's
# copy grows with what the node stores.
wait_root() {
echo "Waiting for root device $ROOTDEV..."
TIMEOUT=${ROOT_TIMEOUT:-1800}
WAITED=0
while [ ! -b "$ROOTDEV" ] && [ $TIMEOUT -gt 0 ]; do
    if ! kill -0 "$STORMBLOCK_PID" 2>/dev/null; then
        echo "  the storage engine (PID $STORMBLOCK_PID) exited before $ROOTDEV appeared"
        break
    fi
    sleep 1
    TIMEOUT=$((TIMEOUT - 1))
    WAITED=$((WAITED + 1))
    # Say something while a long first boot is working, so a wait that is
    # doing something is distinguishable from one that is not.
    case $WAITED in 30|60|120|180|240|300|600|900|1200|1500)
        echo "  still waiting for $ROOTDEV (${WAITED}s) - the engine is alive; a first boot may be copying to local disk" ;;
    esac
done
}
wait_root

# --- BEGIN engine report (covered by tests/initramfs-no-appliance.sh)
# What the engine said last, repeated where it is read: after the FATAL, not
# a screen above it (#294).
engine_report() { # lines
    echo "The storage engine's last ${1:-25} line(s) ($ENGINE_LOG):"
    if [ -s "$ENGINE_LOG" ]; then
        tail -n "${1:-25}" "$ENGINE_LOG" | sed 's/^/  | /'
    else
        echo "  (it wrote nothing)"
    fi
    # The console gets the engine's warnings and stage lines; everything it
    # logged is in its record (#243).
    _rec="${STORM_ENGINE_RECORD:-/run/stormblock/stormblock.log}"
    [ -s "$_rec" ] && echo "  Everything it logged, INFO included: $_rec"
    return 0
}
# --- END engine report

# --- BEGIN root fallback (covered by tests/initramfs-root-fallback.sh)
# A local disk whose root does not come up (#244). server1 on 11.56: the
# disk was held - the release the appliance assigns, by volume id - so it
# booted, and its root would not mount (`erofs: cannot find valid erofs
# superblock`): the boot stopped at a shell with the release one claim away.
#
# Once per boot, when this boot came from a local disk (not a hook's
# decision), an appliance is known and the machine's name is not a guess
# (#249: a guessed name's image may be another machine's): stop the engine,
# boot the image claimed for this machine, and install it over the disk the
# way a release it does not hold is installed. Its system half is laid
# again whatever the ids say (`STORMBLOCK_RELAY_SYSTEM_HALF`); its data half
# is kept (#311). A second failure stops as before. Answers 0 when a new
# engine is running, 1 (with why) when this boot does not fall back.
ROOT_FALLBACK_DONE=""
# Running, as opposed to gone or exited and not yet waited for: `kill -0`
# answers yes for a zombie, and nothing here waits for the engine.
pid_alive() { # pid
    _st=$(sed 's/^.*) //' "/proc/$1/stat" 2>/dev/null) || return 1
    [ -n "$_st" ] && [ "${_st%% *}" != Z ]
}
root_fallback() { # why
    echo "ROOT FAILED: $1"
    if [ -n "$ROOT_FALLBACK_DONE" ]; then
        echo "  already the fallback this boot - stopping here"; return 1
    fi
    if [ "${BOOT_MODE:-}" != local ] || [ -z "${LOCAL_ROOT_SLAB:-}" ]; then
        echo "  this root was not a local disk's - nothing to fall back to"; return 1
    fi
    if [ -z "${BOOTHOST:-}" ]; then
        echo "  no appliance (${BOOTHOST_WHY:-none was found}) - nothing to fall back to"; return 1
    fi
    if identity_guessed 2>/dev/null; then
        echo "  NOT FALLING BACK: $BOOTTAG is a guess from SMBIOS, and its image may be another"
        echo "  machine's (#249)"
        return 1
    fi
    ROOT_FALLBACK_DONE=1
    if [ -z "${CLAIMED:-}" ] && ! boothost_claim; then
        echo "  $BOOTHOST gave this machine no image - stopping here"; return 1
    fi
    case "$LOCAL_ROOT_SLAB" in
    /dev/nvme*p[0-9]*) INSTALL_OVER="${LOCAL_ROOT_SLAB%p[0-9]*}" ;;
    /dev/sd*[0-9])     INSTALL_OVER="${LOCAL_ROOT_SLAB%%[0-9]*}" ;;
    *)                 INSTALL_OVER="$LOCAL_ROOT_SLAB" ;;
    esac
    echo "  FALLING BACK: booting the image $BOOTHOST assigns ($CLAIMED) and installing"
    echo "  it over $INSTALL_OVER: its system half laid again, its data half kept (#244, #311)"
    # The engine that served the failed root stands down first: its devices
    # go (SIGTERM tears the ublk exports down, #105), and the new engine
    # numbers its own from 0.
    kill "$STORMBLOCK_PID" 2>/dev/null
    _w=0
    while pid_alive "$STORMBLOCK_PID" && [ $_w -lt 15 ]; do sleep 1; _w=$((_w + 1)); done
    kill -9 "$STORMBLOCK_PID" 2>/dev/null
    _w=0
    while [ -e "$ROOTDEV" ] && [ $_w -lt 15 ]; do sleep 1; _w=$((_w + 1)); done
    [ -e "$ROOTDEV" ] && echo "  WARNING: $ROOTDEV is still there after the engine stopped"
    kill "$FOLLOW_PID" 2>/dev/null
    unset STORMBLOCK_RESUME_SOURCE
    export STORMBLOCK_RELAY_SYSTEM_HALF=1
    SLAB="$CLAIMED"
    LOCAL_ROOT_SLAB=""
    launch_local
    STORMBLOCK_PID=$!
    tail -n +1 -f "$ENGINE_LOG" 2>/dev/null &
    FOLLOW_PID=$!
    return 0
}
# --- END root fallback

if [ ! -b "$ROOTDEV" ]; then
    root_fallback "root device $ROOTDEV did not appear" && wait_root
fi

if [ ! -b "$ROOTDEV" ]; then
    sleep 1   # the follower's last lines first
    echo "FATAL: root device $ROOTDEV not found after ${WAITED}s"
    echo "StormBlock PID: $STORMBLOCK_PID"
    echo "Available block devices:"
    ls -la /dev/ublk* 2>/dev/null || echo "  (none)"
    engine_report 25
    rescue_shell
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
    if ! mount_root "$ROOTDEV" /run/stormblock/lower; then
        if root_fallback "$ROOTDEV would not mount" && wait_root && [ -b "$ROOTDEV" ]; then
            mount_root "$ROOTDEV" /run/stormblock/lower \
                || { echo "FATAL: Failed to mount overlay lower (the claimed image too)"; rescue_shell; }
        else
            echo "FATAL: Failed to mount overlay lower"; rescue_shell
        fi
    fi

    case "$OVERLAY" in
        tmpfs|tmpfs:*)
            SIZE="${OVERLAY#tmpfs}"; SIZE="${SIZE#:}"
            mount -t tmpfs -o "size=${SIZE:-512m}" tmpfs /run/stormblock/rw \
                || { echo "FATAL: Failed to mount overlay tmpfs"; rescue_shell; }
            ;;
        *)
            TIMEOUT=15
            while [ ! -b "$OVERLAY" ] && [ $TIMEOUT -gt 0 ]; do
                sleep 1; TIMEOUT=$((TIMEOUT - 1))
            done
            mount "$OVERLAY" /run/stormblock/rw \
                || { echo "FATAL: Failed to mount overlay upper $OVERLAY"; rescue_shell; }
            ;;
    esac
    mkdir -p /run/stormblock/rw/upper /run/stormblock/rw/work
    mount -t overlay overlay \
        -o lowerdir=/run/stormblock/lower,upperdir=/run/stormblock/rw/upper,workdir=/run/stormblock/rw/work \
        /sysroot \
        || { echo "FATAL: Failed to mount overlay root"; rescue_shell; }
else
    if ! mount_root "$ROOTDEV" /sysroot; then
        # A root that is there and will not mount (#244).
        if root_fallback "$ROOTDEV would not mount" && wait_root && [ -b "$ROOTDEV" ]; then
            echo "Root device ready: $ROOTDEV (the claimed image)"
            mount_root "$ROOTDEV" /sysroot \
                || { echo "FATAL: Failed to mount root (the claimed image too)"; rescue_shell; }
        else
            echo "FATAL: Failed to mount root"; rescue_shell
        fi
    fi
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
# --- BEGIN container mounts (covered by tests/initramfs-container-mounts.sh)
# In parallel (#302): they were mounted one at a time, ~130-200 ms each, 8.6-
# 10.5 s of every boot for 63 volumes (stormcos#300). Each entry is its own
# ublk device and its own mount point, so nothing orders them but nesting: a
# mount point inside another is mounted in a later wave than its parent
# (waves by depth), never over it. At most STORM_MOUNT_PARALLEL at once. The
# devices are waited for once, for the whole list. `-t ext4` first (every
# list volume is ext4 today, and probing tries erofs and the rest first), a
# probing mount if that fails (an XFS volume).
if [ -n "$MOUNT_MAP" ]; then
    echo "Mounting container volumes..."
    _cm_root="${STORM_SYSROOT:-/sysroot}"
    _cm_par="${STORM_MOUNT_PARALLEL:-16}"
    _cm_wait="${STORM_MOUNT_WAIT:-15}"
    _cm_mount="${STORM_MOUNT:-mount}"
    _cm_dt="${STORM_DEV_TEST:--b}"
    _cm_dir="${STORM_RUN:-/run}/stormblock-mounts"
    mkdir -p "$_cm_dir"
    printf '%s\n' "$MOUNT_MAP" | awk 'NF >= 2 { print $1, $2 }' > "$_cm_dir/list"
    _cm_t0=$(cut -d' ' -f1 /proc/uptime 2>/dev/null)

    # Every device, once: present, or the wait has run out.
    _cm_n=0
    while :; do
        _cm_missing=0
        while read -r mdev mmnt; do
            [ "$_cm_dt" "$mdev" ] || _cm_missing=$((_cm_missing + 1))
        done < "$_cm_dir/list"
        [ "$_cm_missing" -eq 0 ] && break
        [ "$_cm_n" -ge "$_cm_wait" ] && break
        sleep 1
        _cm_n=$((_cm_n + 1))
    done

    # The ones present, by depth (the number of / in the mount point).
    : > "$_cm_dir/present"
    while read -r mdev mmnt; do
        if [ "$_cm_dt" "$mdev" ]; then
            echo "$mdev $mmnt" >> "$_cm_dir/present"
        else
            echo "  WARNING: $mdev never appeared; $mmnt will be empty"
        fi
    done < "$_cm_dir/list"
    awk '{ d = gsub("/", "/", $2); print d, $1, $2 }' "$_cm_dir/present" \
        | sort -n -k1,1 > "$_cm_dir/waves"

    cm_one() { # dev mountpoint
        mkdir -p "$_cm_root$2"
        if $_cm_mount -t ext4 "$1" "$_cm_root$2" 2>/dev/null \
            || $_cm_mount "$1" "$_cm_root$2" 2>/dev/null; then
            echo "  mounted: $1 -> $2"
        else
            echo "  WARNING: $1 would not mount at $2"
        fi
    }
    # Named pids: this shell is PID 1, and a bare `wait` would wait for the
    # engine too.
    _cm_jobs=""; _cm_run=0; _cm_depth=""; _cm_total=0
    while read -r depth mdev mmnt; do
        if [ "$depth" != "$_cm_depth" ] || [ "$_cm_run" -ge "$_cm_par" ]; then
            [ -n "$_cm_jobs" ] && wait $_cm_jobs
            _cm_jobs=""; _cm_run=0; _cm_depth="$depth"
        fi
        cm_one "$mdev" "$mmnt" &
        _cm_jobs="$_cm_jobs $!"
        _cm_run=$((_cm_run + 1))
        _cm_total=$((_cm_total + 1))
    done < "$_cm_dir/waves"
    [ -n "$_cm_jobs" ] && wait $_cm_jobs
    _cm_t1=$(cut -d' ' -f1 /proc/uptime 2>/dev/null)
    if [ -n "$_cm_t0" ] && [ -n "$_cm_t1" ]; then
        echo "  $_cm_total volume(s) mounted in $(awk -v a="$_cm_t0" -v b="$_cm_t1" 'BEGIN { printf "%.1f", b - a }') s ($_cm_par at a time)"
    fi
    rm -rf "$_cm_dir"
fi
# --- END container mounts

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
        echo "  WARNING: $ISDEV never appeared - preloaded images will NOT be available"
    fi
fi

# Verify systemd exists in the new root
if [ ! -x /sysroot/sbin/init ] && [ ! -x /sysroot/usr/lib/systemd/systemd ]; then
    echo "FATAL: No init found in /sysroot"
    rescue_shell
fi

# --- BEGIN install config write (covered by tests/initramfs-install-config.sh)
# The staged install-config.yaml goes to /state on a first boot only: when
# /state is mounted and holds none yet, so booting old media again never
# overwrites a node's applied config (#275). stormpump applies it from there
# (stormpump#78). The staged copy is removed either way: /run moves into the
# real root at switch_root.
if [ -n "${INSTALL_CONFIG_STAGED:-}" ] && [ -f "$INSTALL_CONFIG_STAGED" ]; then
    _icst="${STORM_SYSROOT:-/sysroot}/state"
    if ! awk -v m="$_icst" '$2 == m { f = 1 } END { exit !f }' "${STORM_MOUNTS_FILE:-/proc/mounts}"; then
        echo "install-config: /state is not mounted - not written (the boot media still has it)"
    elif [ -e "$_icst/config/install-config.yaml" ]; then
        echo "install-config: /state already has one - kept, the media's is not applied again"
    elif mkdir -p "$_icst/config" && ( umask 077; cp "$INSTALL_CONFIG_STAGED" "$_icst/config/install-config.yaml" ) \
         && chmod 600 "$_icst/config/install-config.yaml"; then
        echo "install-config: written to /state/config/install-config.yaml (first boot)"
    else
        echo "install-config: could not write /state/config/install-config.yaml"
        rm -f "$_icst/config/install-config.yaml"
    fi
    rm -f "$INSTALL_CONFIG_STAGED"
fi
# --- END install config write

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
    echo "  WARNING: no modules golden at /usr/lib/kernel - this node has only the"
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
    echo "No address yet - retrying with the full firmware set"
    mount --bind /sysroot/usr/lib/kernel/lib/firmware /lib/firmware 2>/dev/null || true
    for ma in /sys/bus/*/devices/*/modalias; do
        [ -f "$ma" ] || continue
        modprobe -q "$(cat "$ma")" 2>/dev/null || true
    done
    for dev in /sys/class/net/*; do
        n=$(basename "$dev"); [ "$n" = "lo" ] && continue
        ip link set "$n" up 2>/dev/null
        udhcpc -i "$n" -s /usr/share/udhcpc/default.script -q -n -t 5 ${DHCP_HOST_ARGS:-} >/dev/null 2>&1 && break
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

# The init that follows gets /dev/console, as it always did; the fan-out goes
# on for as long as the engine writes to it.
console_restore

# switch_root — PID 1 becomes /sbin/init, stormblock continues in background
exec switch_root /sysroot /sbin/init
INITSCRIPT
chmod +x "$INITRD_DIR/init"

# CPU microcode, as an early uncompressed cpio ahead of the real one.
#
# The kernel applies microcode before it brings up the other CPUs, and the
# only way to hand it any that early is a plain cpio prepended to the
# initramfs holding `kernel/x86/microcode/{GenuineIntel,AuthenticAMD}.bin`.
# It must be uncompressed and it must be first; the kernel consumes it and
# hands the remainder to the real initramfs, so nothing else changes.
#
# Without it a node runs whatever its BIOS shipped, for the life of the
# machine. The R230 this was written for reported:
#
#   x86/CPU: Running old microcode        (BIOS 2.4.3, 31 Jan 2018)
#   MDS/TAA/SRBDS/MMIO Stale Data/GDS: Vulnerable ... no microcode
#
# — five mitigations the CPU cannot apply, every one of them shipped after
# that BIOS. Updating firmware fixes one machine once; this fixes every node
# that boots the image, and versions the microcode with the image.
#
# Absent on the build host it is skipped with a warning rather than failing:
# an image without microcode still boots, and a build that stops because a
# firmware package is missing helps nobody.
UCODE_DIR="$INITRD_DIR.ucode"
rm -rf "$UCODE_DIR"
mkdir -p "$UCODE_DIR/kernel/x86/microcode"
UCODE_FOUND=""
if ls /lib/firmware/intel-ucode/* >/dev/null 2>&1; then
    # Every file concatenated: the kernel walks the blob and picks the one
    # matching this CPU's family/model/stepping, so one image serves any of
    # them. It is ~5 MB, and it is not compressed on purpose.
    cat /lib/firmware/intel-ucode/* > "$UCODE_DIR/kernel/x86/microcode/GenuineIntel.bin"
    UCODE_FOUND="$UCODE_FOUND Intel($(ls /lib/firmware/intel-ucode | wc -l) revisions)"
fi
if ls /lib/firmware/amd-ucode/*.bin >/dev/null 2>&1; then
    cat /lib/firmware/amd-ucode/*.bin > "$UCODE_DIR/kernel/x86/microcode/AuthenticAMD.bin"
    UCODE_FOUND="$UCODE_FOUND AMD"
fi

echo ""
echo "Building cpio archive..."
if [ -n "$UCODE_FOUND" ]; then
    echo "  early microcode:$UCODE_FOUND"
    ( cd "$UCODE_DIR" && find . | cpio -o -H newc --quiet 2>/dev/null ) > "$OUTPUT"
else
    echo "  WARNING: no CPU microcode on this build host — nodes will run whatever"
    echo "           their BIOS shipped. Install microcode_ctl and amd-ucode-firmware."
    : > "$OUTPUT"
fi
cd "$INITRD_DIR"
find . | cpio -o -H newc --quiet 2>/dev/null | zstd -19 -T0 >> "$OUTPUT"
rm -rf "$UCODE_DIR"

echo ""
echo "Built: $OUTPUT"
echo "  Size: $(du -h "$OUTPUT" | cut -f1)"
echo ""
echo "Contents:"
echo "  /init                      — LinuxBoot init script"
if ls "$INITRD_DIR/etc/stormblock/boot.d/"* >/dev/null 2>&1; then
    for h in "$INITRD_DIR/etc/stormblock/boot.d/"*; do
        echo "  /etc/stormblock/boot.d/$(basename "$h") — boot hook, asked before the local-slab probe"
    done
else
    echo "  /etc/stormblock/boot.d/    — empty: no boot hook, the local-slab probe decides"
fi
echo "  /usr/sbin/stormblock       — $(du -h "$INITRD_DIR/usr/sbin/stormblock" | cut -f1) static binary"
echo "  /bin/busybox               — $(du -h "$INITRD_DIR/bin/busybox" | cut -f1) shell + tools"
if ls "$INITRD_DIR/lib/modules/"*ublk_drv* >/dev/null 2>&1; then
    echo "  /lib/modules/*ublk_drv*    — kernel module"
fi
echo ""
echo "Boot kernel cmdline:"
echo "  iSCSI: rd.stormblock.portal=<ip> rd.stormblock.iqn=<iqn> rd.stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest"
echo "  netboot: root=/dev/ublkb0   — both parameters below are optional:"
echo "           rd.stormblock.boothost=<url>  overrides DHCP option 17, then http://boothost:9090"
echo "           the name: the firmware's StormBootTag EFI variable (stormbootx), else"
echo "           rd.stormblock.tag=<tag>, else the SMBIOS serial/UUID - a guess, which"
echo "           installs over no disk carrying a slab unless rd.stormblock.trust-smbios=1"
echo "           [rd.stormblock.hostnqn=<nqn>]  — the name firmware presented, echoed on every connect"
echo "           — claims boothost/<tag> and uses the namespace it names as the slab"
echo "  local: root=/dev/ublkb0 rd.stormblock.slab=<dev-or-file-or-nvme-tcp://...> [rd.stormblock.meta=<dir>] [stormblock.volume=<uuid-or-name>]"
echo "         — a hook in /etc/stormblock/boot.d may answer this instead: see docs/boot-hooks.md,"
echo "           and BOOT_HOOKS=\"/path/to/hook ...\" to install one into this image"
echo "         [rd.stormblock.overlay=tmpfs[:SIZE]|<blockdev>]  — writable overlay over a read-only (erofs) root"
echo "         [rd.stormblock.mount=<vol>:<path>,...]  — export and MOUNT these into the real root"
echo "                 for a PID 1 that is not systemd (stormpump reads directories, not fstab)"
