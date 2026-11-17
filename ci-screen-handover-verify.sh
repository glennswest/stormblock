#!/bin/bash
# The boot screen across switch_root (#638), in QEMU.
#
# The guest's /init does what the real one does at the hand-over: the screen
# told (SIGUSR1) and waited for (screen.follow), /proc /sys /dev /run moved
# into a tmpfs /sysroot, then busybox switch_root. The new root's init writes
# stormpump's boot-progress events (stormpump#237) ending in `ready done`.
# The display, read back with QEMU's screendump, must show the READY panel,
# which only a screen that followed into the new root and read its
# /run/stormpump can draw. The picture is left as SCREEN_PNG.
#
#   sc-build 'bash ci-screen-handover-verify.sh'
#
# Needs: qemu-system-x86_64, /boot/vmlinuz-$(uname -r), busybox, python3,
# cargo with the musl target. No root.
set -euo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
[ -r "$KERNEL" ] || { echo "ERROR: cannot read $KERNEL"; exit 1; }
BB=$(command -v busybox) || { echo "ERROR: no busybox"; exit 1; }
QEMU=$(command -v qemu-system-x86_64) || { echo "ERROR: no qemu-system-x86_64"; exit 1; }

echo "== the stormblock binary (musl)"
cargo build --locked --release --target x86_64-unknown-linux-musl --bin stormblock 2>&1 | tail -1
BIN="${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/stormblock"

W=$(mktemp -d)
# Only QEMU's own pid: an empty one must never become `kill 0`.
trap '[ -n "${QPID:-}" ] && kill "$QPID" 2>/dev/null; rm -rf "$W"' EXIT
R=$W/root
mkdir -p "$R"/{bin,sbin,proc,sys,dev,tmp,sysroot,run/stormblock,newroot/bin,newroot/sbin}
cp "$BB" "$R/bin/busybox"
for c in sh mount sleep cat echo ls mkdir kill rm switch_root cp ln; do ln -s busybox "$R/bin/$c"; done
cp "$BIN" "$R/sbin/stormblock"
NOW=$(date +%s)
printf '%s\tstage\tstarting\trunning\tthe installer'"'"'s page\n%s\tstep\tinventory\tdone\t1 drive(s)\n%s\tstep\tflow-over\trunning\tthe system onto sda\n' \
    "$NOW" "$NOW" "$NOW" > "$R/run/stormblock/install-progress.events"
echo "boot" > "$R/run/stormblock/init.log"

# The node's init: stormpump's events, then READY.
cat > "$R/newroot/sbin/init" <<'EOF'
#!/bin/sh
echo "NEW-ROOT-INIT" > /dev/ttyS0
mkdir -p /run/stormpump
E=/run/stormpump/boot-progress.events
t() { date +%s; }
for s in pid1 early install-config modules coldplug-units network boot-manifest coldplug; do
    printf '%s\tstep\t%s\tdone\t\n' "$(t)" "$s" >> $E
done
printf '%s\tservice\tstormblock\tready\t\n' "$(t)" >> $E
printf '%s\tstep\tall-running\tdone\t\n' "$(t)" >> $E
sleep 2
printf '%s\tstep\tready\tdone\tconsole http://192.168.31.203:9094/\n' "$(t)" >> $E
echo "SWITCHED-READY" > /dev/ttyS0
sleep 3600
EOF
chmod +x "$R/newroot/sbin/init"

cat > "$R/init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
mount -t tmpfs run /run
mkdir -p /run/stormblock
cp /run.seed/stormblock/* /run/stormblock/ 2>/dev/null
echo pvetest3 > /proc/sys/kernel/hostname
# The node's root: a tmpfs mount with busybox and its init.
mount -t tmpfs root /sysroot
mkdir -p /sysroot/bin /sysroot/sbin /sysroot/proc /sysroot/sys /sysroot/dev /sysroot/run /sysroot/etc
cp /bin/busybox /sysroot/bin/busybox
for c in sh mkdir sleep date echo printf; do ln -s busybox /sysroot/bin/$c; done
cp /newroot/sbin/init /sysroot/sbin/init
/sbin/stormblock screen > /dev/ttyS0 2>&1 &
SP=$!
sleep 4
# /init's screen_switch_root (#638).
rm -f /run/stormblock/screen.follow
kill -USR1 $SP
w=0; while [ ! -e /run/stormblock/screen.follow ] && [ $w -lt 20 ]; do sleep 0.1; w=$((w+1)); done
[ -e /run/stormblock/screen.follow ] && echo "SCREEN-FOLLOWS" > /dev/ttyS0
mount --move /proc /sysroot/proc
mount --move /sys /sysroot/sys
mount --move /dev /sysroot/dev
mount --move /run /sysroot/run
exec switch_root /sysroot /sbin/init
EOF
chmod +x "$R/init"
mkdir -p "$R/run.seed" && mv "$R/run/stormblock" "$R/run.seed/stormblock"
( cd "$R" && find . | cpio -o -H newc --quiet | gzip -1 ) > "$W/initrd.gz"

ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
echo "== QEMU ($ACCEL): $KERNEL, vga=0x318"
"$QEMU" -accel "$ACCEL" -m 1024 -smp 2 -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 vga=0x318 panic=-1" -vga std -display none -no-reboot \
    -serial "file:$W/serial.log" -monitor "unix:$W/mon,server,nowait" &
QPID=$!
for _ in $(seq 1 240); do
    grep -q SWITCHED-READY "$W/serial.log" 2>/dev/null && break
    sleep 0.5
done
sleep 3
grep -aE "screen:|SCREEN-FOLLOWS|NEW-ROOT-INIT|SWITCHED-READY" "$W/serial.log" || true
grep -q SWITCHED-READY "$W/serial.log" || { tail -30 "$W/serial.log"; echo "ERROR: the guest did not reach the new root's READY"; exit 1; }
python3 - "$W/mon" "$W" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); time.sleep(0.3); s.recv(65536)
s.sendall(("screendump %s/ready.ppm\n" % sys.argv[2]).encode()); time.sleep(2); s.sendall(b"quit\n"); time.sleep(0.5)
PY
wait "$QPID" 2>/dev/null || :
QPID=""
OUT=${SCREEN_PNG:-$PWD/screen-handover.png}
python3 - "$W/ready.ppm" "$OUT" <<'PY'
import sys, zlib, struct
d = open(sys.argv[1], 'rb').read().split(b'\n', 3)
w, h = map(int, d[1].split()); px = d[3]
blue = sum(1 for i in range(0, w*h*3, 3) if abs(px[i]-153) < 12 and abs(px[i+1]-204) < 12 and abs(px[i+2]-255) < 12)
raw = b''.join(b'\x00' + px[y*w*3:(y+1)*w*3] for y in range(h))
def chunk(k, v): return struct.pack('>I', len(v)) + k + v + struct.pack('>I', zlib.crc32(k + v))
open(sys.argv[2], 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b''))
print(f"after the switch: {blue} pale-blue px of {w*h}")
ok = blue > w * h // 8
print("the READY panel, drawn from the new root's /run/stormpump" if ok else "ERROR: no READY panel after the switch")
sys.exit(0 if ok else 1)
PY
grep -aq "SCREEN-FOLLOWS" "$W/serial.log" || { echo "ERROR: the screen did not answer the hand-over"; exit 1; }
grep -aq "screen: following the node's root" "$W/serial.log" || { echo "ERROR: the screen did not enter the new root"; exit 1; }
echo "== the screen followed switch_root and drew stormpump's READY: $OUT"
