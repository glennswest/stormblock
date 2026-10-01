#!/usr/bin/env bash
# The initramfs clock step on a real kernel, as PID 1 (#251).
#
# tests/initramfs-clock.sh runs the block against stubs. This boots dev's
# kernel in QEMU with the guest RTC at 2000-01-01 - an X9 blade after a power
# cut, no RTC battery - and runs the shipped block with busybox's own ntpd,
# date and hwclock over QEMU's user network:
#
#   1. the lease's server (unreachable here) then the fixed addresses: the
#      clock is stepped from 2000, the console line says by how much, and the
#      RTC reads the new time afterwards (hwclock -r)
#   2. the clock put back to 2000 and every server unreachable: the clock is
#      floored at the build date, said loudly, within the bound (2 x 3 s)
#
# Step 1 needs UDP 123 from dev to the internet; without it the step is
# reported SKIP and step 2 still runs.
#
# Needs: qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its virtio_net
# module, a static busybox. Unprivileged. sc-build 'bash ci-clock-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-clock.XXXXXX")
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
for a in ntpd hwclock timeout date; do
    "$BUSYBOX" --list | grep -qx "$a" && ok "busybox has $a" \
        || fail "busybox has no $a applet: the initramfs could not step the clock"
done

say "guest initramfs: busybox, virtio_net, the shipped clock block"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc/stormblock,lib/mods}
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
echo "  modules: $(tr '\n' ' ' < "$I/lib/mods/order")"
BUILT=$(date -u +%s)
echo "$BUILT" > "$I/etc/stormblock/build-date"
BLOCK=$(sed -n '/# --- BEGIN clock step/,/# --- END clock step/p' \
    scripts/build-stormblock-initramfs.sh)
[ -n "$BLOCK" ] || { echo "FAIL: could not extract the clock step block"; exit 1; }
{
    cat <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs tmpfs /run
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up; ip link set eth0 up
ip addr add 10.0.2.15/24 dev eth0; ip route add default via 10.0.2.2
EOF
    printf '%s\n' "$BLOCK"
    cat <<'EOF'
echo "MARK rtc-before $(hwclock -r -u 2>&1)"
echo "MARK date-before $(date +%s)"
# The lease names a server nothing answers on; the fixed addresses follow.
echo 10.0.2.99 > /run/ntp-servers
t0=$(cut -d. -f1 /proc/uptime)
clock_step 10.0.2.15/24 2>&1 | sed 's/^/MARK step1: /'
echo "MARK step1-secs $(( $(cut -d. -f1 /proc/uptime) - t0 ))"
echo "MARK date-after1 $(date +%s)"
echo "MARK rtc-year1 $(hwclock -r -u 2>/dev/null | grep -oE '(19|20)[0-9][0-9]' | tail -1)"
echo "MARK rtc-raw1 $(hwclock -r -u 2>&1)"
# Back to 2000, and nothing answers anywhere.
date -u -s @946684800 >/dev/null
NTP_FALLBACK="10.0.2.98 10.0.2.97"
t0=$(cut -d. -f1 /proc/uptime)
clock_step 10.0.2.15/24 2>&1 | sed 's/^/MARK step2: /'
echo "MARK step2-secs $(( $(cut -d. -f1 /proc/uptime) - t0 ))"
echo "MARK date-after2 $(date +%s)"
echo "MARK done"
poweroff -f
EOF
} > "$I/init"
chmod +x "$I/init"
sh -n "$I/init" || fail "the guest /init does not parse"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU, RTC at 2000-01-01"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 180 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 512 -smp 1 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -rtc base=2000-01-01T00:00:00 \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 > "$W/guest.log" 2>&1
echo "  qemu exit $?"
tr -d '\r' < "$W/guest.log" > "$W/g.txt"
grep '^MARK' "$W/g.txt"
v() { sed -n "s/^MARK $1 //p" "$W/g.txt" | head -1; }

grep -q '^MARK done' "$W/g.txt" || { tail -30 "$W/g.txt"; fail "the guest did not finish"; }
B=$(v date-before)
[ -n "$B" ] && [ "$B" -lt 1000000000 ] && ok "guest booted at $(date -u -d @"$B") (the RTC)" \
    || fail "guest did not boot in 2000: $B"
if grep -q '^MARK step1: clock stepped by +' "$W/g.txt"; then
    ok "step 1: $(sed -n 's/^MARK step1: //p' "$W/g.txt" | head -1)"
    A1=$(v date-after1)
    [ "$A1" -ge "$BUILT" ] && ok "clock after the step is past the build date ($(date -u -d @"$A1"))" \
        || fail "clock after the step: $A1"
    R1=$(v rtc-year1)
    if [ -n "$R1" ] && [ "$R1" -ge "$(date -u -d @"$BUILT" +%Y)" ]; then ok "RTC reads the stepped time ($(v rtc-raw1))"
    else fail "RTC not written: $(v rtc-raw1)"; fi
    grep -q '^MARK step1: .*RTC written' "$W/g.txt" && ok "console says the RTC was written" \
        || fail "no RTC line"
elif grep -q '^MARK step1: WARNING: no time server answered' "$W/g.txt"; then
    echo "  SKIP  step 1: no NTP reachable from dev's QEMU user network"
else
    fail "step 1 said neither: $(grep '^MARK step1' "$W/g.txt" | tr '\n' ' ')"
fi
S2=$(v step2-secs)
grep -q '^MARK step2: WARNING: no time server answered' "$W/g.txt" \
    && ok "step 2: nothing answered, said so" || fail "step 2 did not say nothing answered"
grep -q 'BEFORE THIS IMAGE WAS BUILT' "$W/g.txt" && ok "step 2: floor said loudly" \
    || fail "step 2: no floor message"
A2=$(v date-after2)
[ -n "$A2" ] && [ "$A2" -ge "$BUILT" ] && [ "$A2" -le $((BUILT + 3600)) ] \
    && ok "step 2: clock floored at the build date ($(date -u -d @"$A2"))" \
    || fail "step 2: clock is $A2, build date $BUILT"
[ -n "$S2" ] && [ "$S2" -le 8 ] && ok "step 2: gave up in ${S2}s (bound 2 x 3 s)" \
    || fail "step 2 took ${S2}s"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "-- guest"; tail -40 "$W/g.txt"
echo "FAILURES: $FAILS"; exit 1
