#!/bin/bash
# The boot and install screen on a real DRM display (#618), in QEMU.
#
# Boots the build host's kernel with a linear framebuffer (`vga=0x318`: the
# kernel's own boot code sets a VESA mode, sysfb hands it to simpledrm, the
# same path a UEFI GOP takes on hardware) and an initramfs that runs only
# `stormblock screen` over a sample install record. Then QEMU's own
# `screendump` is read back: the screen must be drawn (the frame's amber and
# lavender, the header text), and `screen: drawing on /dev/dri/card…` must be
# on the serial console. The picture is left as screen.png (SCREEN_PNG names
# where) for a person to look at.
#
#   sc-build 'bash ci-screen-verify.sh'
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
trap 'kill "${QPID:-0}" 2>/dev/null || :; rm -rf "$W"' EXIT
R=$W/root
mkdir -p "$R"/{bin,sbin,proc,sys,dev,tmp,run/stormblock/storage-format}
cp "$BB" "$R/bin/busybox"
for c in sh mount sleep cat echo ls; do ln -s busybox "$R/bin/$c"; done
cp "$BIN" "$R/sbin/stormblock"

# A sample install: an install ticket (the screen says INSTALL), forge1's
# four drives, one already at 4096 + PI and three formatting (#618's "done
# when"), the steps so far.
S=$R/run/stormblock
NOW=$(date +%s)
echo '{"boothost":"http://forge","volume":"v"}' > "$S/install.json"
printf '%s\tstage\tstarting\trunning\tthe installer'"'"'s page on port 9090\n' "$((NOW - 3600))" > "$S/install-progress.events"
printf '%s\tstep\tinventory\tdone\t4 drive(s) on the card\n' "$((NOW - 3590))" >> "$S/install-progress.events"
printf '%s\tstep\tformat\trunning\tsdb sdc sdd\n' "$((NOW - 3500))" >> "$S/install-progress.events"
printf '%s\tstage\tformatting\trunning\tsdb sdc sdd through the card\n' "$((NOW - 3500))" >> "$S/install-progress.events"
for d in sdb:12.3 sdc:11.8 sdd:12.9; do
    n=${d%%:*}; pct=${d#*:}
    echo "/dev/$n: formatting (full) $pct%, about 7h 10m left, 58m so far" > "$S/storage-format/$n.log"
done
echo "/dev/sda: already 4096 + PI type1 (READ CAPACITY 16), not formatted again" > "$S/storage-format/sda.log"
cat > "$S/local-disk.json" <<'EOF'
{"state": "formatting", "drives": [
 {"device": "/dev/sda", "model": "H7210A521SUN010T", "bytes": 9931038130176, "transport": "sas"},
 {"device": "/dev/sdb", "model": "H7210A521SUN010T", "bytes": 9931038130176, "transport": "sas"},
 {"device": "/dev/sdc", "model": "H7210A521SUN010T", "bytes": 9931038130176, "transport": "sas"},
 {"device": "/dev/sdd", "model": "H7210A521SUN010T", "bytes": 9931038130176, "transport": "sas"}]}
EOF
echo "storage: the drives are being formatted (#595)" > "$S/init.log"

cat > "$R/init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
echo "screen-test: /dev/dri: $(ls /dev/dri 2>&1 | tr '\n' ' ')" > /dev/ttyS0
echo forge1 > /proc/sys/kernel/hostname
/sbin/stormblock screen > /dev/ttyS0 2>&1 &
sleep 8
echo SCREEN-SHOT-NOW > /dev/ttyS0
sleep 3600
EOF
chmod +x "$R/init"
( cd "$R" && find . | cpio -o -H newc --quiet | gzip -1 ) > "$W/initrd.gz"

ACCEL=""
[ -w /dev/kvm ] && ACCEL="-enable-kvm -cpu host"
echo "== QEMU: $KERNEL, vga=0x318 (1024x768x24 linear framebuffer)${ACCEL:+, kvm}"
# shellcheck disable=SC2086
"$QEMU" $ACCEL -m 1024 -smp 2 -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 vga=0x318 panic=-1" -vga std -display none \
    -serial "file:$W/serial.log" -monitor "unix:$W/mon,server,nowait" -no-reboot &
QPID=$!
for _ in $(seq 1 240); do
    grep -q SCREEN-SHOT-NOW "$W/serial.log" 2>/dev/null && break
    sleep 0.5
done
grep -aE "screen-test|screen:|simpledrm|fb0|drm" "$W/serial.log" | head -12 || true
grep -q SCREEN-SHOT-NOW "$W/serial.log" || { tail -30 "$W/serial.log"; echo "ERROR: the guest did not get as far as the screen"; exit 1; }
python3 - "$W/mon" "$W/screen.ppm" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); time.sleep(0.3); s.recv(65536)
s.sendall(("screendump %s\n" % sys.argv[2]).encode()); time.sleep(2); s.sendall(b"quit\n"); time.sleep(0.5)
PY
wait "$QPID" 2>/dev/null || :
QPID=""
OUT=${SCREEN_PNG:-$PWD/screen.png}
python3 - "$W/screen.ppm" "$OUT" <<'PY'
import sys, zlib, struct
d = open(sys.argv[1], 'rb').read()
parts = d.split(b'\n', 3)
assert parts[0] == b'P6', parts[0]
w, h = map(int, parts[1].split()); px = parts[3]
assert len(px) >= w * h * 3, (len(px), w, h)
def near(c, r, g, b): return abs(c[0]-r) < 12 and abs(c[1]-g) < 12 and abs(c[2]-b) < 12
amber = lav = lit = 0
for i in range(0, w * h * 3, 3):
    c = px[i:i+3]
    if near(c, 255, 153, 0): amber += 1
    if near(c, 204, 153, 204): lav += 1
    if c[0] + c[1] + c[2] > 60: lit += 1
print(f"screendump {w}x{h}: {amber} amber, {lav} lavender, {lit} lit pixels of {w*h}")
raw = b''.join(b'\x00' + px[y*w*3:(y+1)*w*3] for y in range(h))
def chunk(k, v): return struct.pack('>I', len(v)) + k + v + struct.pack('>I', zlib.crc32(k + v))
open(sys.argv[2], 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b''))
ok = amber > w * h // 50 and lav > w * h // 50
print("the screen is drawn: the frame's amber and lavender" if ok else "ERROR: the screen is not on the display")
sys.exit(0 if ok else 1)
PY
grep -aq "screen: drawing on /dev/dri/card" "$W/serial.log" || { echo "ERROR: no 'screen: drawing on' line"; exit 1; }
grep -a "screen: drawing on" "$W/serial.log"
echo "== the screen, drawn by DRM and read back from the display: $OUT"
