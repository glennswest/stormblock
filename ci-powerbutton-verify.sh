#!/bin/bash
# The power button in the initramfs (#635), in QEMU.
#
# Boots the build host's kernel with an initramfs that runs a stand-in
# storage engine (`boot-local` among its arguments; it says when it is
# stopped) and `stormblock power-button`, then presses the ACPI power button
# from QEMU's monitor (`system_powerdown`: what a BMC's soft off is). The
# guest must say the press, stop the engine, and power itself off, so QEMU
# exits on its own, well inside the 120 s stormcentral waits before cutting.
#
#   sc-build 'bash ci-powerbutton-verify.sh'
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
mkdir -p "$R"/{bin,sbin,proc,sys,dev,tmp,run/stormblock}
cp "$BB" "$R/bin/busybox"
for c in sh mount sleep cat echo ls; do ln -s busybox "$R/bin/$c"; done
cp "$BIN" "$R/sbin/stormblock"
cat > "$R/sbin/fake-engine" <<'EOF'
#!/bin/sh
# Stands in for `stormblock boot-local`: stopped by SIGTERM, which it says.
trap 'echo "ENGINE-STOPPED" > /dev/ttyS0; exit 0' TERM
while :; do sleep 1; done
EOF
chmod +x "$R/sbin/fake-engine"
cat > "$R/init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
/sbin/fake-engine boot-local --slab x &
/sbin/stormblock power-button > /dev/ttyS0 2>&1 &
sleep 4
echo "POWERBUTTON-READY" > /dev/ttyS0
# The rescue shell's stand-in: nothing here answers the button.
sleep 3600
EOF
chmod +x "$R/init"
( cd "$R" && find . | cpio -o -H newc --quiet | gzip -1 ) > "$W/initrd.gz"

ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
echo "== QEMU ($ACCEL): $KERNEL"
"$QEMU" -accel "$ACCEL" -m 512 -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1" -display none -no-reboot \
    -serial "file:$W/serial.log" -monitor "unix:$W/mon,server,nowait" &
QPID=$!
for _ in $(seq 1 240); do
    grep -q POWERBUTTON-READY "$W/serial.log" 2>/dev/null && break
    sleep 0.5
done
grep -q POWERBUTTON-READY "$W/serial.log" || { tail -30 "$W/serial.log"; echo "ERROR: the guest did not get as far as the watcher"; exit 1; }
grep -a "power button" "$W/serial.log" || true
echo "== pressing the ACPI power button (system_powerdown)"
T0=$(date +%s)
python3 - "$W/mon" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); time.sleep(0.3); s.recv(65536)
s.sendall(b"system_powerdown\n"); time.sleep(0.5)
PY
# The guest powering itself off ends QEMU.
for _ in $(seq 1 120); do
    kill -0 "$QPID" 2>/dev/null || break
    sleep 0.5
done
if kill -0 "$QPID" 2>/dev/null; then
    tail -20 "$W/serial.log"
    echo "ERROR: the guest did not power off within 60 s of the button"
    exit 1
fi
QPID=""
echo "the guest powered off $(( $(date +%s) - T0 )) s after the button"
grep -a "power button\|ENGINE-STOPPED" "$W/serial.log"
grep -aq "power button: watching" "$W/serial.log" || { echo "ERROR: the watcher did not find the button"; exit 1; }
grep -aq "power button: pressed" "$W/serial.log" || { echo "ERROR: the press was not said"; exit 1; }
grep -aq "ENGINE-STOPPED" "$W/serial.log" || { echo "ERROR: the engine was not stopped before the power went"; exit 1; }
grep -aq "process(es) stopped in" "$W/serial.log" || { echo "ERROR: no 'stopped in' line"; exit 1; }
echo "== the power button powers the initramfs off cleanly: said, the engine stopped, synced, off"
