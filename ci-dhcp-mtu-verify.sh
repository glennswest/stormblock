#!/usr/bin/env bash
# ci-dhcp-mtu-verify.sh — the DHCP MTU reaches the bridge the root runs over
# (#425), on a real kernel with real busybox DHCP on both ends.
#
# In a QEMU guest: a veth pair, one end a port of `stormbr0` (as the initramfs
# builds it), the other a busybox `udhcpd` offering MTU 9000 (g16's pool).
# `udhcpc -O mtu` with the initramfs's own `default.script` (taken from the
# generator, as built) leases on stormbr0. Then the kernel says: the port and
# the bridge at 9000, the address on, the console line said. And again with
# a declared `[network] mtu` of 1500, which must win on the bridge (the port,
# already at 9000, is never lowered: #545).
#
# Then the early ports (#545), with the generator's own `early ports` and
# `bridge bring-up` blocks: a port down at 1500 is raised to 9000 before it
# goes up, the bridge is pinned at 1500; a 9000 lease takes the bridge to
# 9000 with the port left; a lease with no MTU leaves the bridge at 1500.
#
# Unprivileged on a build VM: root only inside the guest.
#   sc-build 'sh ci-dhcp-mtu-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-dhcp-mtu.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }
"$BUSYBOX" --list | grep -qx udhcpd || { echo "SKIP: this busybox has no udhcpd"; exit 2; }

I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods,usr/share/udhcpc,var/lib/misc}
cp "$BUSYBOX" "$I/bin/busybox"
for a in $("$BUSYBOX" --list); do ln -sf busybox "$I/bin/$a"; done
# iproute2 for the test's own plumbing (busybox `ip` makes no veth pair); the
# DHCP script runs busybox `ip`, as on a node.
IPR=$(command -v ip)
# Named `ip` (iproute2 reads its subcommand from its own name: as `ipr` it
# runs `ip route`), apart from busybox's.
mkdir -p "$I/opt/iproute2"
[ -n "$IPR" ] && cp "$IPR" "$I/opt/iproute2/ip" && ldd "$IPR" 2>/dev/null | grep -o '/[^ ]*' | while read -r lib; do
    mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
done
# The DHCP script exactly as the generator writes it.
sed -n "/<< 'DHCPSCRIPT'/,/^DHCPSCRIPT$/p" scripts/build-stormblock-initramfs.sh | sed '1d;$d' \
    > "$I/usr/share/udhcpc/default.script"
chmod 755 "$I/usr/share/udhcpc/default.script"
grep -q dhcp_mtu "$I/usr/share/udhcpc/default.script" || { echo "FAIL: no dhcp_mtu in the extracted script"; exit 1; }
sed -n '/# --- BEGIN early ports/,/# --- END early ports/p' scripts/build-stormblock-initramfs.sh | grep -v '^early_ports$' > "$I/early.sh"
sed -n '/# --- BEGIN bridge bring-up/,/# --- END bridge bring-up/p' scripts/build-stormblock-initramfs.sh > "$I/bridge.sh"
[ -s "$I/early.sh" ] && [ -s "$I/bridge.sh" ] || { echo "FAIL: could not extract the early ports or bridge blocks"; exit 1; }
# The generator passes -O mtu everywhere; the guest does what /init does.
grep -q 'udhcpc -i "$IFACE" -O mtu' scripts/build-stormblock-initramfs.sh || fail "the generator starts udhcpc without -O mtu"

: > "$I/lib/mods/order"
for m in veth bridge; do
    modprobe -S "$KVER" --show-depends "$m" 2>/dev/null | awk '$1=="insmod"{print $2}'
done | awk '!seen[$0]++' | while read -r ko; do
    base=$(basename "$ko" | sed 's/\.xz$//; s/\.zst$//')
    case "$ko" in
        *.xz) xz -dc "$ko" > "$I/lib/mods/$base" ;;
        *.zst) zstd -dcq "$ko" > "$I/lib/mods/$base" ;;
        *) cp "$ko" "$I/lib/mods/$base" ;;
    esac
    echo "$base" >> "$I/lib/mods/order"
done
cat > "$I/init" <<'EOF'
#!/bin/sh
export PATH=/bin
ipr() { /opt/iproute2/ip "$@"; }
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
r() { echo "RESULT $1 $2"; }
ip link set lo up
echo "LOG ipr: $(ipr -V 2>&1)"
ipr link add srv type veth peer name port0 2>&1 | sed 's/^/LOG veth: /'
echo "LOG modules: $(cut -d' ' -f1 /proc/modules | tr '\n' ' ')"
ipr link set srv mtu 9000; ipr link set srv up
ipr addr add 192.168.30.252/20 dev srv
ipr link add stormbr0 type bridge 2>&1 | sed 's/^/LOG bridge: /' 
ipr link set port0 master stormbr0
ipr link set port0 up; ipr link set stormbr0 up
cat > /etc/udhcpd.conf <<EOT
start 192.168.30.2
end 192.168.30.20
interface srv
lease_file /var/lib/misc/udhcpd.leases
opt subnet 255.255.240.0
opt router 192.168.16.1
opt mtu 9000
opt lease 600
EOT
touch /var/lib/misc/udhcpd.leases
udhcpd -f /etc/udhcpd.conf > /run/udhcpd.log 2>&1 &
sleep 1
echo "GUEST before: port0 $(cat /sys/class/net/port0/mtu), stormbr0 $(cat /sys/class/net/stormbr0/mtu)"

lease() {
    udhcpc -i stormbr0 -O mtu -s /usr/share/udhcpc/default.script -q -n -t 5 > /run/udhcpc.log 2>&1
    sed 's/^/GUEST udhcpc: /' /run/udhcpc.log
}
lease
p=$(cat /sys/class/net/port0/mtu); b=$(cat /sys/class/net/stormbr0/mtu)
echo "GUEST leased: port0 $p, stormbr0 $b, $(ip -4 addr show stormbr0 | grep -o 'inet [^ ]*')"
[ "$p" = 9000 ] && r port PASS || r port "FAIL ($p)"
[ "$b" = 9000 ] && r bridge PASS || r bridge "FAIL ($b)"
ip -4 addr show stormbr0 | grep -q 'inet 192.168.30.' && r address PASS || r address FAIL
grep -q "mtu 9000 on stormbr0 (from DHCP)" /run/udhcpc.log && r said PASS || r said FAIL

# A declared [network] mtu wins over the lease's.
echo 1500 > /run/declared-mtu
lease
p=$(cat /sys/class/net/port0/mtu); b=$(cat /sys/class/net/stormbr0/mtu)
echo "GUEST declared: port0 $p, stormbr0 $b"
[ "$p" = 9000 ] && [ "$b" = 1500 ] && r declared PASS || r declared "FAIL ($p/$b)"
rm -f /run/declared-mtu

# The early ports (#545). A veth has no `device` link, so early_ports reads a
# tree naming the real port, with the kernel's own mtu and flags; it sets
# through busybox ip, on the real port.
netsay() { echo "$*"; }
early() { # port: a fresh bridge over it, the port down at 1500, then early ports and the bridge
    ipr link del stormbr0 2>/dev/null
    ipr link set "$1" nomaster 2>/dev/null; ipr link set "$1" down; ipr link set "$1" mtu 1500
    rm -rf /run/fsys /run/stormblock-port-mtu; mkdir -p "/run/fsys/class/net/$1"
    : > "/run/fsys/class/net/$1/device"
    cat "/sys/class/net/$1/mtu" > "/run/fsys/class/net/$1/mtu"
    cat "/sys/class/net/$1/flags" > "/run/fsys/class/net/$1/flags"
    STORM_SYSFS=/run/fsys
    . /early.sh
    early_ports | sed 's/^/GUEST early: /'
    BRIDGE=stormbr0; NO_BRIDGE=""
    . /bridge.sh
    net_bring_up "$1" | sed 's/^/GUEST bridge: /'
}
early port0
p=$(cat /sys/class/net/port0/mtu); b=$(cat /sys/class/net/stormbr0/mtu)
o=$(cat /sys/class/net/port0/operstate)
echo "GUEST early: port0 $p ($o), stormbr0 $b, kept $(cat /run/stormblock-port-mtu/port0 2>/dev/null)"
[ "$p" = 9000 ] && [ "$b" = 1500 ] && r early-raised PASS || r early-raised "FAIL ($p/$b)"
[ "$(cat /run/stormblock-port-mtu/port0 2>/dev/null)" = 1500 ] && r early-kept PASS || r early-kept FAIL
lease
p=$(cat /sys/class/net/port0/mtu); b=$(cat /sys/class/net/stormbr0/mtu)
echo "GUEST early, 9000 lease: port0 $p, stormbr0 $b"
[ "$p" = 9000 ] && [ "$b" = 9000 ] && r early-jumbo PASS || r early-jumbo "FAIL ($p/$b)"
ip -4 addr show stormbr0 | grep -q 'inet 192.168.30.' && r early-address PASS || r early-address FAIL

# A network whose lease names no MTU: the bridge stays at the port's own.
ipr link add srv2 type veth peer name port1
ipr link set srv2 up; ipr addr add 192.168.40.252/24 dev srv2
sed 's/^interface srv$/interface srv2/; s/^start .*/start 192.168.40.2/; s/^end .*/end 192.168.40.20/; /^opt mtu/d; /^opt router/d; s#udhcpd.leases#udhcpd2.leases#' /etc/udhcpd.conf > /etc/udhcpd2.conf
touch /var/lib/misc/udhcpd2.leases
udhcpd -f /etc/udhcpd2.conf > /run/udhcpd2.log 2>&1 &
sleep 1
ipr link del stormbr0 2>/dev/null
early port1
lease
p=$(cat /sys/class/net/port1/mtu); b=$(cat /sys/class/net/stormbr0/mtu)
echo "GUEST early, no lease MTU: port1 $p, stormbr0 $b, $(ip -4 addr show stormbr0 | grep -o 'inet [^ ]*')"
[ "$p" = 9000 ] && [ "$b" = 1500 ] && r early-plain PASS || r early-plain "FAIL ($p/$b)"
ip -4 addr show stormbr0 | grep -q 'inet 192.168.40.' && r early-plain-address PASS || r early-plain-address FAIL

# No bridge: the address on the port itself, which gets its own MTU back.
ipr link del stormbr0 2>/dev/null
ipr link set port1 nomaster; ipr link set port1 down; ipr link set port1 mtu 1500
rm -rf /run/stormblock-port-mtu; mkdir -p /run/fsys/class/net/port1
cat /sys/class/net/port1/flags > /run/fsys/class/net/port1/flags; echo 1500 > /run/fsys/class/net/port1/mtu
early_ports > /dev/null
NO_BRIDGE=1 net_bring_up port1 | sed 's/^/GUEST nobridge: /'
p=$(cat /sys/class/net/port1/mtu)
echo "GUEST no bridge: port1 $p"
[ "$p" = 1500 ] && r early-nobridge PASS || r early-nobridge "FAIL ($p)"
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 300 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 512 -smp 2 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG)|panick'
for c in port bridge address said declared early-raised early-kept early-jumbo early-address early-plain early-plain-address early-nobridge; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $c PASS" || fail "$c"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
