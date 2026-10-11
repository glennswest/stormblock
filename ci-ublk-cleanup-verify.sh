#!/usr/bin/env bash
# ci-ublk-cleanup-verify.sh — a ublk test never leaves its device behind
# (#135). Unprivileged on the build VM: the test runs as root inside a QEMU
# guest, where ublk_drv is its own.
#
# On dev a test that died with /dev/ublkb0 up left it quiesced with no server
# for a day, and every `sync` on the box hung behind it. The test that makes
# ublk devices on a host, tests-runtime's `ublk_resize`, now takes its device
# down on every way out (a guard), and sweeps devices whose server is gone
# before it starts. In the guest, the test binary is run four times:
#
#   clean    the test passes; no ublk device is left
#   panic    it panics with its device up (STORMBLOCK_TEST_UBLK_PANIC): the
#            test fails, the guard removes the device, none is left
#   abort    it aborts with its device up (STORMBLOCK_TEST_UBLK_ABORT):
#            nothing runs, the device is left, its server pid gone
#   sweep    the next run removes that orphan first (says so), then passes;
#            none is left
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) (6.12+, for
# the resize the test also checks) and its ublk_drv, a static busybox.
#   sc-build 'bash ci-ublk-cleanup-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-ublk-cleanup.XXXXXX")
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build the ublk_resize test"
TEST=$(cargo test --locked --release -p stormblock-runtime-tests --test ublk_resize --no-run 2>&1 \
    | sed -n 's/.*Executable .*(\(.*\))/\1/p' | tail -1)
[ -x "$TEST" ] || { echo "FAIL: no test binary"; exit 1; }
echo "test binary: $TEST"

say "guest initramfs: busybox, the test binary, ublk_drv"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod sleep cat echo ls grep poweroff dmesg tail wc sed; do
    ln -sf busybox "$I/bin/$a"
done
cp "$TEST" "$I/bin/ublk_resize"
ldd "$TEST" | grep -o '/[^ ]*' | while read -r lib; do
    mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
done
: > "$I/lib/mods/order"
modprobe -S "$KVER" --show-depends ublk_drv 2>/dev/null | awk '$1=="insmod"{print $2}' | awk '!seen[$0]++' \
    | while read -r ko; do
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
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
export TMPDIR=/tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
left() { ls /sys/class/ublk-char 2>/dev/null | wc -l; }
run() { # name, env: the test once, its exit, its output kept
    env $2 /bin/ublk_resize --ignored --exact a_live_ublk_device_follows_its_volume_growing \
        --test-threads 1 > /tmp/$1.out 2>&1
    echo $? > /tmp/$1.rc
    grep -E "SWEEP|GUARD|panicked|test result" /tmp/$1.out | sed "s/^/LOG $1: /"
}

run clean ""
[ "$(cat /tmp/clean.rc)" = 0 ] && r clean-passes PASS || r clean-passes "FAIL (exit $(cat /tmp/clean.rc))"
[ "$(left)" = 0 ] && r clean-leaves-nothing PASS || r clean-leaves-nothing "FAIL ($(ls /sys/class/ublk-char))"

run panic "STORMBLOCK_TEST_UBLK_PANIC=1"
[ "$(cat /tmp/panic.rc)" != 0 ] && r panic-fails-the-test PASS || r panic-fails-the-test FAIL
[ "$(left)" = 0 ] && r panic-leaves-nothing PASS || r panic-leaves-nothing "FAIL ($(ls /sys/class/ublk-char))"

run abort "STORMBLOCK_TEST_UBLK_ABORT=1"
n=$(left)
[ "$n" = 1 ] && r abort-leaves-an-orphan PASS || r abort-leaves-an-orphan "FAIL ($n devices)"

run sweep ""
grep -q "SWEEP removed /dev/ublkb" /tmp/sweep.out && r sweep-removes-the-orphan PASS || r sweep-removes-the-orphan FAIL
[ "$(cat /tmp/sweep.rc)" = 0 ] && r sweep-then-passes PASS || r sweep-then-passes "FAIL (exit $(cat /tmp/sweep.rc))"
[ "$(left)" = 0 ] && r sweep-leaves-nothing PASS || r sweep-leaves-nothing "FAIL ($(ls /sys/class/ublk-char))"

echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 600 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 2048 -smp 2 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=3" > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG)'
for m in clean-passes clean-leaves-nothing panic-fails-the-test panic-leaves-nothing abort-leaves-an-orphan \
         sweep-removes-the-orphan sweep-then-passes sweep-leaves-nothing; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
