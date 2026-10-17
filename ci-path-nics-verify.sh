#!/usr/bin/env bash
# ci-path-nics-verify.sh — the initramfs's path NICs (#450) on a real kernel
# with real busybox DHCP on both ends.
#
# In a QEMU guest: a "switch" (bridge `lan`, a busybox `udhcpd` on it) and two
# veth links into it. port0 is the uplink, a port of `stormbr0`, leased with
# the initramfs's own default.script, as /init does. port1 is cabled too. The
# `path nics` block, extracted from the generator as built, then runs with
# the real busybox ip and udhcpc and its own path.script:
#   port1 leased; its subnet route at metric 1024 and no default route by it,
#   so the uplink's routes are what unbound traffic uses; arp_ignore=1 and
#   arp_announce=2; the lease handed over in /run/stormblock/path-nics/port1.json;
#   rd.stormblock.path-skip=port1 leaves it alone.
#
# Unprivileged on a build VM: root only inside the guest.
#   sc-build 'sh ci-path-nics-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-path-nics.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }
"$BUSYBOX" --list | grep -qx udhcpd || { echo "SKIP: this busybox has no udhcpd"; exit 2; }

GEN=scripts/build-stormblock-initramfs.sh
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods,usr/share/udhcpc,var/lib/misc,opt/iproute2}
cp "$BUSYBOX" "$I/bin/busybox"
for a in $("$BUSYBOX" --list); do ln -sf busybox "$I/bin/$a"; done
# iproute2 for the test's own plumbing (busybox `ip` makes no veth pair); the
# block and the DHCP scripts run busybox `ip`, as on a node.
IPR=$(command -v ip)
[ -n "$IPR" ] && cp "$IPR" "$I/opt/iproute2/ip" && ldd "$IPR" 2>/dev/null | grep -o '/[^ ]*' | while read -r lib; do
    mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
done
# What the generator writes, as built.
sed -n "/<< 'DHCPSCRIPT'/,/^DHCPSCRIPT$/p" "$GEN" | sed '1d;$d' > "$I/usr/share/udhcpc/default.script"
sed -n "/<< 'PATHSCRIPT'/,/^PATHSCRIPT$/p" "$GEN" | sed '1d;$d' > "$I/usr/share/udhcpc/path.script"
sed -n '/# --- BEGIN path nics/,/# --- END path nics/p' "$GEN" > "$I/path-nics.sh"
chmod 755 "$I/usr/share/udhcpc/default.script" "$I/usr/share/udhcpc/path.script"
for f in usr/share/udhcpc/default.script usr/share/udhcpc/path.script path-nics.sh; do
    [ -s "$I/$f" ] || { echo "FAIL: could not extract $f"; exit 1; }
done

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
# The switch, its DHCP server, and two cables into it.
ipr link add lan type bridge; ipr link set lan up
ipr addr add 192.168.30.252/20 dev lan
for n in 0 1; do
    ipr link add port$n type veth peer name lp$n
    ipr link set lp$n master lan; ipr link set lp$n up
done
cat > /etc/udhcpd.conf <<EOT
start 192.168.30.2
end 192.168.30.20
interface lan
lease_file /var/lib/misc/udhcpd.leases
opt subnet 255.255.240.0
opt router 192.168.16.1
opt dns 192.168.30.252
opt lease 600
EOT
touch /var/lib/misc/udhcpd.leases
udhcpd -f /etc/udhcpd.conf > /run/udhcpd.log 2>&1 &
sleep 1
# The node: port0 its uplink on stormbr0, as /init builds it.
ipr link add stormbr0 type bridge
ipr link set port0 master stormbr0
ipr link set port0 up; ipr link set port1 up; ipr link set stormbr0 up
udhcpc -i stormbr0 -O mtu -s /usr/share/udhcpc/default.script -q -n -t 5 > /run/uplink.log 2>&1
echo "GUEST uplink: $(ip -4 addr show stormbr0 | grep -o 'inet [^ ]*')"
cp /etc/resolv.conf /tmp/resolv.before 2>/dev/null; touch /tmp/resolv.before
sleep 1
# The block, as /init runs it.
netsay() { echo "$@"; }
NET_SYSFS=/sys/class/net
net_carrier() { cat "$NET_SYSFS/$1/carrier" 2>/dev/null || echo 0; }
pn() {
    ALL_PHYS="port0 port1" LEASED=port0 IFACE=stormbr0
    . /path-nics.sh
}
pn > /run/pn.log 2>&1
sed 's/^/GUEST block: /' /run/pn.log
a=$(ip -4 addr show port1 | grep -o 'inet [^ ]*')
echo "GUEST port1: $a"
echo "GUEST routes: $(ipr -4 route | tr '\n' ';')"
case "$a" in *192.168.30.*) r leased PASS ;; *) r leased "FAIL ($a)" ;; esac
ipr -4 route | grep -E 'dev port1 .*metric 1024' | grep -q 'scope link' && r route-metric PASS || r route-metric FAIL
ipr -4 route | grep -q 'default .*dev port1' && r no-default "FAIL" || r no-default PASS
ipr -4 route | grep -q 'default .*dev stormbr0' && r uplink-default PASS || r uplink-default FAIL
cmp -s /etc/resolv.conf /tmp/resolv.before && r resolv-untouched PASS || r resolv-untouched FAIL
[ "$(cat /proc/sys/net/ipv4/conf/all/arp_ignore)" = 1 ] && [ "$(cat /proc/sys/net/ipv4/conf/port1/arp_announce)" = 2 ] \
    && r arp PASS || r arp FAIL
h=/run/stormblock/path-nics/port1.json
echo "GUEST handover: $(cat $h 2>/dev/null)"
grep -q '"interface": "port1"' $h 2>/dev/null && grep -q '"server": "192.168.30.252"' $h && grep -q '"lease_secs": 600' $h \
    && r handover PASS || r handover FAIL
grep -q 'path NIC port1: 192.168.30.' /run/pn.log && r said PASS || r said FAIL
# Opted out: nothing done.
ip addr flush dev port1; rm -f $h
STORMBLOCK_PATH_SKIP=port1 pn > /run/pn2.log 2>&1
ip -4 addr show port1 | grep -q inet && r skip "FAIL (leased)" || r skip PASS
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
for c in leased route-metric no-default uplink-default resolv-untouched arp handover said skip; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $c PASS" || fail "$c"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
