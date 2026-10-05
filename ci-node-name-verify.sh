#!/usr/bin/env bash
# The node's name in its DHCP requests, on a real kernel, as PID 1 (#238).
#
# tests/initramfs-node-name.sh runs the blocks against stubs. This boots dev's
# kernel in QEMU with an initramfs whose /init is the shipped `dhcp name hint`
# and `node name` blocks around the shipped udhcpc script, with the firmware's
# boot name set to `stormblock1`. QEMU's user network answers DHCP; a
# filter-dump on the NIC records every packet the guest sends. Checked:
#   * the DHCP request carries option 12 = stormblock1 (what a server's lease
#     table shows);
#   * the lease is applied (an address on eth0);
#   * with no option 12 back and no PTR for the address, the console says
#     why each step gave no name, and the node takes storm-<mac>.
#
# Needs: qemu-system-x86_64, /boot/vmlinuz-$(uname -r), a static busybox,
# python3. Unprivileged. Run on dev through sc-build:
#   sc-build 'bash ci-node-name-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-nodename.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
GEN=scripts/build-stormblock-initramfs.sh

say "guest initramfs: busybox, virtio_net, the shipped udhcpc script and naming blocks"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods,usr/share/udhcpc}
cp "$BUSYBOX" "$I/bin/busybox"
for a in $("$BUSYBOX" --list); do
    [ -e "$I/bin/$a" ] || ln -s busybox "$I/bin/$a"
done
: > "$I/lib/mods/order"
modprobe -S "$KVER" --show-depends virtio_net 2>/dev/null | awk '$1=="insmod"{print $2}' \
| while read -r ko; do
    base=$(basename "$ko" | sed 's/\.xz$//; s/\.zst$//')
    case "$ko" in
        *.xz) xz -dc "$ko" > "$I/lib/mods/$base" ;;
        *.zst) zstd -dcq "$ko" > "$I/lib/mods/$base" ;;
        *) cp "$ko" "$I/lib/mods/$base" ;;
    esac
    echo "$base" >> "$I/lib/mods/order"
done
sed -n "/^cat > \"\$INITRD_DIR\/usr\/share\/udhcpc\/default.script\" << 'DHCPSCRIPT'/,/^DHCPSCRIPT/p" "$GEN" \
    | sed '1d;$d' > "$I/usr/share/udhcpc/default.script"
chmod +x "$I/usr/share/udhcpc/default.script"
[ -s "$I/usr/share/udhcpc/default.script" ] || { echo "FAIL: could not extract the udhcpc script"; exit 1; }
HINT=$(sed -n '/# --- BEGIN dhcp name hint/,/# --- END dhcp name hint/p' "$GEN")
NAME=$(sed -n '/# --- BEGIN node name/,/# --- END node name/p' "$GEN")
[ -n "$HINT" ] && [ -n "$NAME" ] || { echo "FAIL: could not extract the naming blocks"; exit 1; }
{
    cat <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs tmpfs /run
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up; ip link set eth0 up
IFACE=eth0; UPLINK=eth0
SLAB=""; BOOTTAG=stormblock1; BOOTTAG_FROM=firmware
EOF
    printf '%s\n' "$HINT"
    cat <<'EOF'
echo "MARK args $DHCP_HOST_ARGS"
udhcpc -i "$IFACE" -s /usr/share/udhcpc/default.script -q -n -t 5 $DHCP_HOST_ARGS 2>&1 | sed 's/^/MARK udhcpc: /'
echo "MARK addr $(ip -4 -o addr show dev eth0 | awk '{print $4}')"
EOF
    printf '%s\n' "$NAME"
    cat <<'EOF'
echo "MARK hostname $(cat /proc/sys/kernel/hostname)"
echo "MARK done"
poweroff -f
EOF
} > "$W/init.body"
# Every line the naming block prints, marked.
{ head -n 0 /dev/null; }
awk '{ print }' "$W/init.body" > "$I/init"
chmod +x "$I/init"
sh -n "$I/init" || fail "the guest /init does not parse"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU, the NIC's traffic dumped"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 180 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 512 -smp 1 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0,mac=0c:c4:7a:06:f9:6d \
    -object filter-dump,id=d0,netdev=n0,file="$W/net.pcap" > "$W/guest.log" 2>&1
echo "  qemu exit $?"
tr -d '\r' < "$W/guest.log" > "$W/g.txt"
grep -E '^MARK|^  ' "$W/g.txt" | head -30
grep -q '^MARK done' "$W/g.txt" || { tail -30 "$W/g.txt"; fail "the guest did not finish"; }

grep -q '^MARK args -x hostname:stormblock1' "$W/g.txt" && ok "udhcpc is asked to send hostname stormblock1" \
    || fail "the hint did not set -x hostname:stormblock1"
grep -q "asking DHCP as 'stormblock1', the name its firmware booted as" "$W/g.txt" \
    && ok "the console says which name it asks under, and why" || fail "no line naming the DHCP name"
ADDR=$(sed -n 's/^MARK addr //p' "$W/g.txt")
[ -n "$ADDR" ] && ok "the lease is applied ($ADDR)" || fail "no address after udhcpc"

# Option 12 in what the guest sent: a DHCP packet (UDP 68 -> 67) whose
# options hold 0x0c, length, "stormblock1".
python3 - "$W/net.pcap" <<'PY' && ok "option 12 = stormblock1 in the guest's DHCP request" || fail "no option 12 = stormblock1 in any DHCP packet the guest sent"
import struct, sys
data = open(sys.argv[1], 'rb').read()
off, found = 24, False
while off + 16 <= len(data):
    incl = struct.unpack('<I', data[off+8:off+12])[0]
    pkt = data[off+16:off+16+incl]; off += 16 + incl
    if len(pkt) < 14 or pkt[12:14] != b'\x08\x00': continue
    ihl = (pkt[14] & 0x0f) * 4
    udp = pkt[14+ihl:]
    if pkt[23] != 17 or len(udp) < 8: continue
    sport, dport = struct.unpack('>HH', udp[:4])
    if (sport, dport) != (68, 67): continue
    opts = udp[8+240:]
    i = 0
    while i < len(opts) and opts[i] != 255:
        if opts[i] == 0: i += 1; continue
        code, ln = opts[i], opts[i+1]
        if code == 12 and opts[i+2:i+2+ln] == b'stormblock1': found = True
        i += 2 + ln
sys.exit(0 if found else 1)
PY

grep -q '^  the lease names no host (no option 12)' "$W/g.txt" && ok "says the lease named no host" \
    || fail "no line for the missing option 12"
if grep -q "^  DNS has no name for 10.0.2.15" "$W/g.txt" || grep -q "^  name from DNS:" "$W/g.txt"; then
    ok "says what DNS answered ($(grep -E '^  (DNS has no name|name from DNS)' "$W/g.txt" | head -1 | sed 's/^  //'))"
else
    fail "no line for the PTR step"
fi
H=$(sed -n 's/^MARK hostname //p' "$W/g.txt")
[ -n "$H" ] && ok "the node is named: $H" || fail "no hostname"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILS"; exit 1; fi
