#!/usr/bin/env bash
# The install path's log over the network, on a real kernel, as PID 1 (#477).
#
# tests/initramfs-logcast.sh runs the blocks with a stand-in logcast, and
# `logcast::tests` the follower on loopback. This boots the build VM's kernel
# in QEMU with an initramfs whose /init is the shipped console fan-out,
# logcast and engine follower blocks, the shipped udhcpc script and node name
# block, and this commit's static `stormblock`. Before the network is up it
# prints 300 lines and an "engine" writes 50 to its log; then it brings eth0
# up by DHCP (QEMU's user network), prints more, and stops logcast as /init
# does before switch_root. Checked:
#   * a collector on the host (`stormpump.syslog=10.0.2.2:<port>`, QEMU
#     forwards it to 127.0.0.1) receives every line, in order: /init's as
#     APP-NAME stormblock-init, the engine's as stormblock, each once, with
#     the node's name as HOSTNAME;
#   * the guest also sent them to the stormcast group 239.255.42.1:5514
#     (filter-dump on the NIC);
#   * every frame is RFC 5424 as stormcast's docs/wire.md has it;
#   * with `stormpump.syslog=off`, nothing at all leaves the guest.
#
# Needs: qemu-system-x86_64, /boot/vmlinuz-$(uname -r), a static busybox,
# python3, the musl target. Unprivileged:
#   sc-build 'bash ci-logcast-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-logcast.XXXXXX")
COLLECTOR_PID=""
trap '[ -n "$COLLECTOR_PID" ] && kill "$COLLECTOR_PID" 2>/dev/null; rm -rf "$W"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
GEN=scripts/build-stormblock-initramfs.sh

say "this commit's static stormblock"
command -v rustup >/dev/null 2>&1 && rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1
cargo build --release --locked --target x86_64-unknown-linux-musl -p stormblock --bin stormblock 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no $BIN"; exit 1; }

say "guest initramfs: busybox, virtio_net, stormblock, the shipped blocks"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods,usr/share/udhcpc,usr/sbin}
cp "$BUSYBOX" "$I/bin/busybox"
for a in $("$BUSYBOX" --list); do
    [ -e "$I/bin/$a" ] || ln -s busybox "$I/bin/$a"
done
cp "$BIN" "$I/usr/sbin/stormblock"
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
BLOCKS=""
for b in "console fan-out" "logcast" "engine follower"; do
    BLOCKS="$BLOCKS
$(sed -n "/# --- BEGIN $b/,/# --- END $b/p" "$GEN")"
done
NAME=$(sed -n '/# --- BEGIN node name/,/# --- END node name/p' "$GEN")
echo "$BLOCKS" | grep -q logcast_start && [ -n "$NAME" ] || { echo "FAIL: could not extract the blocks"; exit 1; }
{
    cat <<'EOF'
#!/bin/sh
export PATH=/usr/sbin:/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs tmpfs /run
ENGINE_LOG=/run/stormblock/engine.log
EOF
    printf '%s\n' "$BLOCKS"
    cat <<'EOF'
mkdir -p /run/stormblock; : > "$ENGINE_LOG"
engine_follow_start
i=1; while [ $i -le 300 ]; do echo "early line $i of 300, before the network"; i=$((i + 1)); done
i=1; while [ $i -le 50 ]; do echo "engine: extent $i moved" >> "$ENGINE_LOG"; i=$((i + 1)); done
echo "ERROR: a line the collector files as an error"
sleep 1
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up; ip link set eth0 up
IFACE=eth0; UPLINK=eth0; SLAB=""; BOOTTAG=""; BOOTTAG_FROM=""
udhcpc -i "$IFACE" -s /usr/share/udhcpc/default.script -q -n -t 5 >/dev/null 2>&1
echo "MARK addr $(ip -4 -o addr show dev eth0 | awk '{print $4}')"
EOF
    printf '%s\n' "$NAME"
    cat <<'EOF'
echo "MARK hostname $(cat /proc/sys/kernel/hostname)"
echo "late line, after the network"
sleep 2
logcast_stop
echo "MARK status $(tr '\n' '|' < /run/stormblock/logcast.status)"
echo "MARK done"
poweroff -f
EOF
} > "$I/init"
chmod +x "$I/init"
sh -n "$I/init" || fail "the guest /init does not parse"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

# The collector: every datagram on a loopback port, one per line.
PORT=$(python3 -c 'import socket; s=socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 - "$PORT" "$W/frames" <<'PY' &
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 8 << 20)
s.bind(("127.0.0.1", int(sys.argv[1])))
with open(sys.argv[2], "ab", buffering=0) as f:
    while True:
        d = s.recv(65536)
        f.write(d.replace(b"\n", b" ") + b"\n")
PY
COLLECTOR_PID=$!
sleep 0.5

boot() { # cmdline extra, pcap
    ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
    : > "$W/frames"
    timeout 240 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 512 -smp 2 \
        -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
        -append "console=ttyS0 panic=-1 loglevel=4 $1" \
        -netdev user,id=n0 -device virtio-net-pci,netdev=n0,mac=0c:c4:7a:06:f9:6d \
        -object filter-dump,id=d0,netdev=n0,file="$2" > "$W/guest.log" 2>&1
    echo "  qemu exit $?"
    tr -d '\r' < "$W/guest.log" > "$W/g.txt"
    grep '^MARK' "$W/g.txt"
    grep -q '^MARK done' "$W/g.txt" || { tail -30 "$W/g.txt"; fail "the guest did not finish"; }
    sleep 0.5
}

say "boot with stormpump.syslog=10.0.2.2:$PORT"
boot "stormpump.syslog=10.0.2.2:$PORT" "$W/net.pcap"
H=$(sed -n 's/^MARK hostname //p' "$W/g.txt")
[ -n "$H" ] && ok "the node is named: $H" || fail "no hostname"
grep -q '^early line 300 of 300' "$W/g.txt" && ok "the console still has /init's lines" || fail "the console lost /init's lines"
grep -q '^engine: extent 1 moved' "$W/g.txt" && ok "the console has the engine's lines" || fail "the console lost the engine's lines"

python3 - "$W/frames" "$W/net.pcap" "$H" "$PORT" <<'PY' || fail "the frames"
import re, struct, sys
frames = [l.rstrip("\n") for l in open(sys.argv[1], errors="replace") if l.strip()]
host, port = sys.argv[3], int(sys.argv[4])
wire = re.compile(r"^<(\d{1,3})>1 (\S+) (\S+) (\S+) - - - (.*)$")
bad = [f for f in frames if not wire.match(f)]
fails = 0
def check(ok, what):
    global fails
    print(("  ok    " if ok else "FAIL: ") + what)
    if not ok: fails += 1
check(not bad, f"{len(frames)} frame(s) at the collector, every one RFC 5424 ({bad[:2]})")
parsed = [wire.match(f).groups() for f in frames if wire.match(f)]
init = [m for (_, _, _, t, m) in parsed if t == "stormblock-init"]
eng = [m for (_, _, _, t, m) in parsed if t == "stormblock"]
early = [m for m in init if m.startswith("early line ")]
check(early == [f"early line {i} of 300, before the network" for i in range(1, 301)],
      f"all 300 lines from before the network, in order, once ({len(early)})")
check(eng == [f"engine: extent {i} moved" for i in range(1, 51)], f"all 50 engine lines as APP-NAME stormblock, once ({len(eng)})")
check(not any(m.startswith("engine: ") for m in init), "the engine's lines are not also in /init's (the console's copy)")
check("late line, after the network" in init, "a line after the network")
check(all(h == host for (_, _, h, _, _) in parsed), f"HOSTNAME is the node's name, {host}")
err = [p for (p, _, _, _, m) in parsed if m.startswith("ERROR: a line")]
check(err == ["131"], f"an ERROR line has PRI 131 (local0.err): {err}")
check(any(t == "stormblock-logcast" and "the boot's log to 10.0.2.2" in m for (_, _, _, t, m) in parsed), "logcast says where it sends")

# The same lines to the group, in what the guest sent.
data = open(sys.argv[2], "rb").read()
off, group = 24, 0
while off + 16 <= len(data):
    incl = struct.unpack("<I", data[off+8:off+12])[0]
    pkt = data[off+16:off+16+incl]; off += 16 + incl
    if len(pkt) < 34 or pkt[12:14] != b"\x08\x00" or pkt[23] != 17: continue
    ihl = (pkt[14] & 0x0f) * 4
    dst = pkt[30:34]; udp = pkt[14+ihl:]
    if dst == bytes([239, 255, 42, 1]) and struct.unpack(">H", udp[2:4])[0] == 5514 and b"early line " in udp[8:]:
        group += 1
check(group == 300, f"the 300 early lines also went to 239.255.42.1:5514 ({group})")
sys.exit(1 if fails else 0)
PY

say "boot with stormpump.syslog=off"
boot "stormpump.syslog=off" "$W/off.pcap"
python3 - "$W/frames" "$W/off.pcap" <<'PY' || fail "off sent something"
import struct, sys
n = sum(1 for l in open(sys.argv[1]) if l.strip())
data = open(sys.argv[2], "rb").read()
off, logs = 24, 0
while off + 16 <= len(data):
    incl = struct.unpack("<I", data[off+8:off+12])[0]
    pkt = data[off+16:off+16+incl]; off += 16 + incl
    if len(pkt) < 34 or pkt[12:14] != b"\x08\x00" or pkt[23] != 17: continue
    ihl = (pkt[14] & 0x0f) * 4
    if struct.unpack(">H", pkt[14+ihl+2:14+ihl+4])[0] in (5514, 514): logs += 1
print(f"  {'ok   ' if n == 0 and logs == 0 else 'FAIL:'} off: {n} frame(s) at the collector, {logs} syslog datagram(s) on the wire")
sys.exit(0 if n == 0 and logs == 0 else 1)
PY
grep -q 'nothing sent over the network (syslog off' "$W/g.txt" && ok "off: the status says so" || fail "off: no status line"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILS"; exit 1; fi
