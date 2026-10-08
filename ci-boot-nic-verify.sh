#!/usr/bin/env bash
# A declared static boot-NIC address, on a real kernel, as PID 1 (#229).
#
# tests/initramfs-boot-nic.sh runs the blocks against stubs. This boots dev's
# kernel in QEMU with an initramfs whose /init is the shipped `uplink
# selection`, `bridge bring-up` and `boot nic` blocks, and a stormcos.toml
# whose [network] puts 10.0.2.15/24 on eth0 statically (gateway 10.0.2.2,
# mtu 1400), with `ip=dhcp` on the command line as every release has it.
# QEMU's user network would answer DHCP; a filter-dump records every packet
# the guest sends. Checked:
#   * the address is on stormbr0, eth0 is its port, mtu 1400, the default
#     route via the declared gateway, resolv.conf from the declaration;
#   * the guest reaches the host through that gateway (an HTTP fetch);
#   * the guest sent no DHCP packet at all.
# A second boot declares a port the machine does not have: no static address,
# and the console says why.
#
# Needs: qemu-system-x86_64, /boot/vmlinuz-$(uname -r) with its modules, a
# static busybox, python3. Unprivileged. Run on dev through sc-build:
#   sc-build 'bash ci-boot-nic-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-bootnic.XXXXXX")
HTTP_PID=""
trap '[ -n "$HTTP_PID" ] && kill "$HTTP_PID" 2>/dev/null; rm -rf "$W"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
GEN=scripts/build-stormblock-initramfs.sh

say "a file for the guest to fetch through its gateway"
mkdir -p "$W/www"
echo "reached-over-the-static-address" > "$W/www/ok"
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$W/www" > "$W/http.log" 2>&1 &
HTTP_PID=$!

say "guest initramfs: busybox, virtio_net, bridge, the shipped network blocks"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in $("$BUSYBOX" --list); do
    [ -e "$I/bin/$a" ] || ln -s busybox "$I/bin/$a"
done
: > "$I/lib/mods/order"
for mod in virtio_net bridge; do
    modprobe -S "$KVER" --show-depends "$mod" 2>/dev/null | awk '$1=="insmod"{print $2}'
done | awk '!seen[$0]++' | while read -r ko; do
    base=$(basename "$ko" | sed 's/\.xz$//; s/\.zst$//')
    case "$ko" in
        *.xz) xz -dc "$ko" > "$I/lib/mods/$base" ;;
        *.zst) zstd -dcq "$ko" > "$I/lib/mods/$base" ;;
        *) cp "$ko" "$I/lib/mods/$base" ;;
    esac
    echo "$base" >> "$I/lib/mods/order"
done
echo "  modules: $(tr '\n' ' ' < "$I/lib/mods/order")"
SEL=$(sed -n '/# --- BEGIN uplink selection/,/# --- END uplink selection/p' "$GEN")
BR=$(sed -n '/# --- BEGIN bridge bring-up/,/# --- END bridge bring-up/p' "$GEN")
NIC=$(sed -n '/# --- BEGIN boot nic/,/# --- END boot nic/p' "$GEN")
[ -n "$SEL" ] && [ -n "$BR" ] && [ -n "$NIC" ] || { echo "FAIL: could not extract the network blocks"; exit 1; }

{
    cat <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs tmpfs /run
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up
netsay() { echo "$@"; }
BRIDGE=stormbr0
IP_CONF=dhcp
DECLARED_NET=""
STATE_DIR=/run/state; mkdir -p "$STATE_DIR"
STATE_TOML="$STATE_DIR/stormcos.toml"; STATE_NODE="$STATE_DIR/install-node.toml"
STATE_DISK=/dev/sda
DECL_IF=$(sed -n 's/.*declif=\([^ ]*\).*/\1/p' /proc/cmdline)
cat > "$STATE_TOML" <<TOML
[node]
hostname = "stormblock1"

[network]
interface = "$DECL_IF"
mode = "static"
address = "10.0.2.15/24"
gateway = "10.0.2.2"
dns = ["10.0.2.3"]
domain = "example.lo"
mtu = 1400
TOML
EOF
    printf '%s\n' "$SEL" "$BR" "$NIC"
    cat <<EOF
PORT=$PORT
EOF
    cat <<'EOF'
if [ -n "$BOOT_ADDRS" ]; then
    static_apply
else
    echo "MARK no static"
fi
echo "MARK addr $(ip -4 -o addr show dev stormbr0 2>/dev/null | awk '{print $4}')"
echo "MARK master $(basename "$(readlink /sys/class/net/eth0/master 2>/dev/null)")"
echo "MARK mtu $(cat /sys/class/net/eth0/mtu)"
echo "MARK route $(ip route show default)"
echo "MARK resolv $(tr '\n' ' ' < /etc/resolv.conf 2>/dev/null)"
if [ -n "$BOOT_ADDRS" ]; then
    echo "MARK fetched $(wget -q -T 10 -O - "http://10.0.2.2:$PORT/ok" 2>&1)"
fi
echo "MARK done"
poweroff -f
EOF
} > "$I/init"
chmod +x "$I/init"
sh -n "$I/init" || fail "the guest /init does not parse"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

boot() { # declared interface, log
    ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
    timeout 180 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 512 -smp 1 \
        -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
        -append "console=ttyS0 panic=-1 loglevel=4 ip=dhcp declif=$1" \
        -netdev user,id=n0 -device virtio-net-pci,netdev=n0,mac=0c:c4:7a:06:f9:6d \
        -object filter-dump,id=d0,netdev=n0,file="$W/$2.pcap" > "$W/$2.log" 2>&1
    echo "  qemu exit $?"
    tr -d '\r' < "$W/$2.log" > "$W/$2.txt"
    grep -E '^MARK|^  |WARNING' "$W/$2.txt" | head -30
    grep -q '^MARK done' "$W/$2.txt" || { tail -30 "$W/$2.txt"; fail "the guest did not finish"; }
}
mark() { sed -n "s/^MARK $2 //p" "$W/$1.txt" | head -1; }

say "boot 1: [network] static on eth0, ip=dhcp on the command line"
boot eth0 one
[ "$(mark one addr)" = "10.0.2.15/24" ] && ok "10.0.2.15/24 on stormbr0" || fail "stormbr0 has '$(mark one addr)'"
[ "$(mark one master)" = "stormbr0" ] && ok "eth0 is a port of stormbr0" || fail "eth0's master is '$(mark one master)'"
[ "$(mark one mtu)" = "1400" ] && ok "eth0 mtu 1400" || fail "eth0 mtu '$(mark one mtu)'"
case "$(mark one route)" in
*"via 10.0.2.2"*) ok "default route via the declared gateway" ;;
*) fail "default route '$(mark one route)'" ;;
esac
case "$(mark one resolv)" in
*"search example.lo"*"nameserver 10.0.2.3"*) ok "resolv.conf from the declaration" ;;
*) fail "resolv.conf '$(mark one resolv)'" ;;
esac
grep -q "static: 10.0.2.15/24 on eth0, gateway 10.0.2.2, from \[network\] in stormcos.toml" "$W/one.txt" \
    && ok "the console says what it applied and from where" || fail "no 'static:' line naming the declaration"
[ "$(mark one fetched)" = "reached-over-the-static-address" ] && ok "the host is reached through the gateway" \
    || fail "fetch gave '$(mark one fetched)'"
python3 - "$W/one.pcap" <<'PY' && ok "the guest sent no DHCP packet" || fail "the guest sent DHCP"
import struct, sys
data = open(sys.argv[1], 'rb').read()
off, dhcp, sent = 24, 0, 0
while off + 16 <= len(data):
    incl = struct.unpack('<I', data[off+8:off+12])[0]
    pkt = data[off+16:off+16+incl]; off += 16 + incl
    if len(pkt) < 14 or pkt[6:12] != bytes.fromhex('0cc47a06f96d'): continue
    sent += 1
    if pkt[12:14] != b'\x08\x00' or pkt[23] != 17: continue
    ihl = (pkt[14] & 0x0f) * 4
    udp = pkt[14+ihl:]
    if len(udp) >= 4 and struct.unpack('>HH', udp[:4]) == (68, 67): dhcp += 1
print(f"  the guest sent {sent} frame(s), {dhcp} DHCP")
sys.exit(0 if sent > 0 and dhcp == 0 else 1)
PY

say "boot 2: [network] names a port this machine does not have"
boot eno9 two
grep -q '^MARK no static' "$W/two.txt" && ok "no static address" || fail "a static address was applied"
grep -q "names eno9, which this machine does not have" "$W/two.txt" && ok "the console says why" \
    || fail "no line saying eno9 is not here"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILS"; exit 1; fi
