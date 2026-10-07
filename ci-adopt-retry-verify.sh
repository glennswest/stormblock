#!/usr/bin/env bash
# ci-adopt-retry-verify.sh — adopt-ublk when its restore fails after the
# incumbent has exited (#190). Unprivileged on dev: the engine runs as root
# inside a QEMU guest, where ublk_drv is its own.
#
# In the guest, a volume with known bytes on /dev/vda is served as
# /dev/ublkb0 by `boot-local` (the incumbent), then:
#
#   retry      adopt-ublk whose first two restores fail (the test hook
#              STORMBLOCK_ADOPT_TEST_FAIL_RESTORES=2): it says so on the
#              console, takes the device on the third, and a read started
#              while nothing served it completes with the right bytes
#   give up    a second adopt-ublk whose restores always fail, with a 5 s
#              budget: exit 75, /run/stormblock/adopt-failed.json, the device
#              still there and held — a read started then waits, not fails
#   rerun      adopt-ublk again: it takes the held device, the waiting read
#              completes with the right bytes, the failure record is gone
#   refused    adopt-ublk told to read a slab on /dev/ublkb0: refused before
#              the stand-down, and the server it would have replaced still
#              serves
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its
# ublk_drv module, a static busybox and curl. Run on dev through sc-build:
#   sc-build 'sh ci-adopt-retry-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-adopt.XXXXXX")
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX" "$CURL"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

say "guest initramfs: busybox, stormblock, curl, ublk_drv"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed \
         awk cut tr kill seq mkdir rm timeout pidof ps touch; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
modprobe -S "$KVER" --show-depends ublk_drv 2>/dev/null | awk '$1=="insmod"{print $2}' \
    | awk '!seen[$0]++' | while read -r ko; do
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
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
ip link set lo up
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
api() { curl -sf -m 60 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
log() { sed "s/^/LOG $1: /" "$2" | tail -${3:-12}; }
# Running, not a zombie: this shell is PID 1 and reaps only what it waits for.
alive() { [ -r /proc/$1/stat ] && ! grep -q '^[0-9]* ([^)]*) Z' /proc/$1/stat; }

( sleep ${WATCHDOG:-240}
  echo "WATCHDOG fired"
  for l in /run/*.log; do log "${l##*/}" "$l" 15; done
  ps | sed 's/^/PS /'
  poweroff -f ) &

# 1. A volume with known bytes, written through the daemon, then laid down.
mkdir -p /run/sb
cat > /run/sb.toml <<EOT
[management]
api_token = "t"
listen_addr = "127.0.0.1:9091"
data_dir = "/run/sb"
node_name = "ci-adopt"
discovery_disabled = true
EOT
# The node token may format a slab here (#274's gate, audit mode).
STORMBLOCK_ADMIN_GATE=audit stormblock --config /run/sb.toml --data-dir /run/sb --no-iscsi > /run/daemon.log 2>&1 &
d=$!
for i in $(seq 1 100); do api http://127.0.0.1:9091/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
api -X POST http://127.0.0.1:9091/api/v1/slabs -d '{"device_path":"/dev/vda","role":"system"}' >/dev/null \
    || { r setup "FAIL (slab)"; log daemon /run/daemon.log; poweroff -f; }
id=$(api -X POST http://127.0.0.1:9091/api/v1/volumes -d '{"name":"root","size":"64M"}' \
    | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
dev=$(api -X POST http://127.0.0.1:9091/api/v1/volumes/$id/attach -d '{"transport":"ublk"}' \
    | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
[ -b "$dev" ] || { r setup "FAIL (attach '$dev')"; log daemon /run/daemon.log; poweroff -f; }
dd if=/dev/urandom of=/tmp/pat bs=1M count=4 2>/dev/null
dd if=/tmp/pat of=$dev bs=1M count=4 oflag=direct 2>/dev/null
api -X DELETE http://127.0.0.1:9091/api/v1/volumes/$id/attach >/dev/null
kill -TERM $d; wait $d
echo "GUEST volume root ($id) written; daemon stopped"

# Where boot-local finds the records: in the slab, else the daemon's dir.
META=
stormblock boot-local --slab /dev/vda --volume root --check > /run/check.log 2>&1 \
    || META="--meta /run/sb"
echo "GUEST boot-local ${META:-with the slab's own records}"

# 2. The incumbent.
stormblock boot-local --slab /dev/vda $META --volume root > /run/boot-local.log 2>&1 &
inc=$!
for i in $(seq 1 100); do [ -b /dev/ublkb0 ] && [ -e /run/stormblock/handover.json ] && break; sleep 0.2; done
dd if=/dev/ublkb0 of=/tmp/b0 bs=1M count=4 iflag=direct 2>/dev/null
cmp -s /tmp/pat /tmp/b0 && r incumbent-serves PASS || { r incumbent-serves FAIL; log boot-local /run/boot-local.log; poweroff -f; }

# A read the moment the server is gone: it must wait, then get the bytes.
# Not in $(...): the reader must be this shell's child to be waited for.
read_in_gap() {
    srv=$1; out=$2
    for i in $(seq 1 600); do alive $srv || break; sleep 0.05; done
    dd if=/dev/ublkb0 of=$out bs=1M count=4 iflag=direct >/dev/null 2>&1 &
    RD=$!
}

# 3. retry: two failed restores, then the device is taken.
STORMBLOCK_ADOPT_TEST_FAIL_RESTORES=2 stormblock adopt-ublk > /run/adopt-a.log 2>&1 &
a=$!
read_in_gap $inc /tmp/ra; rd=$RD
for i in $(seq 1 200); do grep -q "Adopted\|adopted" /run/adopt-a.log && break; alive $a || break; sleep 0.1; done
sleep 1
wait $rd 2>/dev/null
grep -c "restore attempt [0-9]* failed" /run/adopt-a.log | sed 's/^/GUEST retry: failures said: /'
if alive $a && cmp -s /tmp/pat /tmp/ra && [ "$(grep -c 'restore attempt [12] failed' /run/adopt-a.log)" = 2 ]; then
    r retry PASS
else
    r retry FAIL; log adopt-a /run/adopt-a.log 20
fi

# 4. give up: every restore fails, 5 s budget.
STORMBLOCK_ADOPT_TEST_FAIL_RESTORES=1000 STORMBLOCK_ADOPT_RESTORE_SECS=5 \
    stormblock adopt-ublk > /run/adopt-b.log 2>&1 &
b=$!
read_in_gap $a /tmp/rb; held=$RD
wait $b; rc=$?
echo "GUEST give-up: exit $rc"
sleep 2
if [ "$rc" = 75 ] && [ -e /run/stormblock/adopt-failed.json ] && [ -b /dev/ublkb0 ] && alive $held \
   && grep -q "FATAL: adopt-ublk gave up" /run/adopt-b.log; then
    r give-up PASS
else
    r give-up "FAIL (exit $rc, marker $([ -e /run/stormblock/adopt-failed.json ] && echo yes || echo no), read $(alive $held && echo waiting || echo ended))"
    log adopt-b /run/adopt-b.log 20
fi
sed 's/^/GUEST marker: /' /run/stormblock/adopt-failed.json 2>/dev/null | head -12

# 5. rerun: takes the held device; the waiting read completes.
stormblock adopt-ublk > /run/adopt-c.log 2>&1 &
c=$!
wait $held 2>/dev/null
sleep 1
if alive $c && cmp -s /tmp/pat /tmp/rb && [ ! -e /run/stormblock/adopt-failed.json ]; then
    r rerun PASS
else
    r rerun "FAIL (adopter $(alive $c && echo up || echo down), marker $([ -e /run/stormblock/adopt-failed.json ] && echo left || echo gone))"
    log adopt-c /run/adopt-c.log 20
fi

# 6. refused: a slab on the ublk device itself is refused before the stand-down.
stormblock adopt-ublk --slab /dev/ublkb0 > /run/adopt-d.log 2>&1
rc=$?
dd if=/dev/ublkb0 of=/tmp/rd bs=1M count=4 iflag=direct 2>/dev/null
if [ "$rc" != 0 ] && [ "$rc" != 75 ] && alive $c && cmp -s /tmp/pat /tmp/rd \
   && grep -q "is on ublk device" /run/adopt-d.log; then
    r refused PASS
else
    r refused "FAIL (exit $rc)"; log adopt-d /run/adopt-d.log 20
fi
grep -h "is on ublk device" /run/adopt-d.log | head -1 | sed 's/^/GUEST refused: /'

kill -TERM $c 2>/dev/null
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
truncate -s 1G "$W/a.img"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 600 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 2048 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -drive file="$W/a.img",if=virtio,format=raw > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG|PS|WATCHDOG)|panick'

for m in incumbent-serves retry give-up rerun refused; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
