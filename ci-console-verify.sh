#!/usr/bin/env bash
# The initramfs console fan-out on a real kernel, as PID 1 (#237).
#
# tests/initramfs-console.sh runs the block against plain files. This boots
# dev's kernel in QEMU with an initramfs whose /init is the shipped block, and
# two serial ports standing in for tty0 and ttyS0:
#
#   console=ttyS1 console=ttyS3 console=ttyS0,115200n8
#
# ttyS0 is last, so it is /dev/console (serial on a release); ttyS1 is the
# screen that saw nothing before; ttyS3 has no UART behind it and must be
# skipped. Each port is captured to a file. Checked: every /init line and
# a background child's (the engine's) reach both ports; after the restore
# before switch_root, /init's lines reach /dev/console only while the child's
# still reach both; the emergency shell prompts on both. /dev/console is
# whichever port the kernel made its console: the 8250 driver has one console
# for all its ports, so here that is ttyS1, the first — not the last console=.
#
# Needs: qemu-system-x86_64, /boot/vmlinuz-$(uname -r), a static busybox.
# Unprivileged. Run on dev through sc-build:  sc-build 'bash ci-console-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-console.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
file -L "$BUSYBOX" | grep -q 'statically linked' || { echo "SKIP: $BUSYBOX is not static"; exit 2; }
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }
for a in mkfifo setsid tee; do
    "$BUSYBOX" --list | grep -qx "$a" || fail "busybox has no $a applet: the initramfs would skip the fan-out"
done

say "guest initramfs: busybox and the shipped console block"
I="$W/initrd"
mkdir -p "$I"/{bin,sbin,usr/sbin,dev,proc,sys,run,tmp}
cp "$BUSYBOX" "$I/bin/busybox"
for a in $("$BUSYBOX" --list); do
    [ -e "$I/bin/$a" ] || ln -s busybox "$I/bin/$a"
done
BLOCK=$(sed -n '/# --- BEGIN console fan-out/,/# --- END console fan-out/p' \
    scripts/build-stormblock-initramfs.sh)
[ -n "$BLOCK" ] || { echo "FAIL: could not extract the console fan-out"; exit 1; }
{
    cat <<'EOF'
#!/bin/sh
export PATH=/usr/sbin:/bin:/sbin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mount -t tmpfs tmpfs /run
EOF
    printf '%s\n' "$BLOCK"
    cat <<'EOF'
echo "MARK consoles: $CONSOLES"
echo "MARK active: $(cat /sys/class/tty/console/active)"
echo "MARK primary: $(console_primary)"
echo "MARK fanout: ${CONSOLE_FANOUT_PID:-none}"
echo "MARK INSTALL: from /init stdout"
echo "MARK FATAL: from /init stderr" >&2
# The engine: started by /init, still writing after the restore.
( sleep 2; echo "MARK Flow-over: from a child after the restore" ) &
# The emergency shell as a FATAL reaches it, with the fan-out on; in the
# background so this can end the run (stdin as /init's, not /dev/null).
( sleep 4; rescue_shell </dev/console ) &
sleep 1
console_restore
echo "MARK after restore: /dev/console only"
sleep 6
echo "MARK done"
poweroff -f
EOF
} > "$I/init"
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 180 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 512 -smp 1 \
    -display none -monitor none -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -serial "file:$W/s0.log" -serial "file:$W/s1.log" \
    -append "console=ttyS1 console=ttyS3 console=ttyS0,115200n8 panic=-1 loglevel=4" \
    > "$W/qemu.log" 2>&1
echo "  qemu exit $?"
for p in s0 s1; do tr -d '\r' < "$W/$p.log" > "$W/$p.txt"; done
echo "-- ttyS0 (/dev/console)"; grep -E 'MARK|#' "$W/s0.txt" | tail -20
echo "-- ttyS1 (the other console)"; grep -E 'MARK|#' "$W/s1.txt" | tail -20

has() { grep -qF -- "$2" "$W/$1.txt"; }
has s0 "MARK done" || has s1 "MARK done" || fail "the guest did not finish (see qemu output below)"
has s0 "MARK consoles: /dev/ttyS1 /dev/ttyS0" \
    && ok "consoles picked: ttyS1 ttyS0, ttyS3 (no UART) skipped" \
    || fail "consoles picked: $(grep 'MARK consoles' "$W/s0.txt")"
# /dev/console is whichever port the kernel made its console; the other is
# the one that saw nothing before #237.
PRIMARY=$(sed -n 's|^MARK primary: /dev/tty||p' "$W/s0.txt" | head -1)
case "$PRIMARY" in
    S0) P=s0; O=s1 ;;
    S1) P=s1; O=s0 ;;
    *)  fail "no primary console reported: '$PRIMARY'"; P=s0; O=s1 ;;
esac
echo "  /dev/console is ttyS${P#s} ($(grep -m1 'MARK active' "$W/s0.txt"))"
for line in "MARK INSTALL: from /init stdout" "MARK FATAL: from /init stderr" \
            "MARK Flow-over: from a child after the restore" "Dropping to shell..."; do
    for p in s0 s1; do
        has "$p" "$line" && ok "$p: $line" || fail "$p lacks '$line'"
    done
done
has "$P" "MARK after restore: /dev/console only" && ok "$P: /init after the restore (/dev/console)" \
    || fail "$P lacks /init's line after the restore"
has "$O" "MARK after restore" && fail "$O has /init's line after the restore" \
    || ok "$O: /init after the restore is not there (as before #237)"
# One emergency shell per console: busybox's root prompt is '~ # ' or '/ # '.
for p in s0 s1; do
    n=$(grep -oE '[~/] # ' "$W/$p.txt" | wc -l)
    [ "$n" -ge 1 ] && ok "$p: emergency shell prompt" || fail "$p: no emergency shell prompt"
done
has "$O" "job control turned off" && fail "$O: the /dev/console shell ran there too" \
    || ok "$O: a shell of its own (setsid, job control)"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "-- qemu"; tail -20 "$W/qemu.log"
echo "FAILURES: $FAILS"; exit 1
