#!/usr/bin/env bash
# ci-ublk-qd-verify.sh — a ublk volume on a slow disk, served one request at a
# time and served at once (#264). Unprivileged on dev: the engine runs as root
# inside a QEMU guest, where ublk_drv and device-mapper are its own.
#
# The guest's disks go through dm-delay (8 ms per read and write, 20 ms per
# flush) so they behave like the 7200 rpm drive of an X9 blade rather than
# dev's storage. On each, the engine (the same binary) serves a volume over
# ublk, once with STORMBLOCK_UBLK_SERIAL=1 (the queue worker as it was: one
# request at a time) and once without, and the guest measures:
#
#   parallel reads   16 readers × 32 random 4 KiB O_DIRECT reads
#   reads under fsync  64 random 4 KiB reads while another process writes
#                    and fsyncs in a loop (a journal commit's FLUSH)
#   round trip       8 writers at distinct offsets, then everything read back
#                    and compared (correctness of concurrent service)
#
# Pass: the round trip is exact in both modes, and served at once is at least
# 3× faster on parallel reads than served serially.
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules
# (ublk_drv, dm-delay), a static busybox, dmsetup and curl (copied in with
# their libraries). Run on dev through sc-build:
#   sc-build 'sh ci-ublk-qd-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
DMSETUP=$(command -v dmsetup || true)
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-ublk-qd.XXXXXX")
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX" "$DMSETUP" "$CURL"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

say "guest initramfs: busybox, stormblock, dmsetup, curl, ublk_drv, dm-delay"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed \
         awk cut tr kill seq mkdir rm; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$DMSETUP" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
for m in ublk_drv dm-delay; do
    modprobe -S "$KVER" --show-depends "$m" 2>/dev/null | awk '$1=="insmod"{print $2}'
done | awk '!seen[$0]++' | while read -r ko; do
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
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
now() { cut -d' ' -f1 /proc/uptime; }
ms() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%d", (b-a)*1000}'; }

# One random 4 KiB block inside the first 64 MiB.
rnd() { echo $(( (RANDOM * 32768 + RANDOM) % 16384 )); }

run() {
    mode=$1; disk=$2; port=$3
    S=$(cat /sys/block/$disk/size)
    dmsetup create --noudevsync slow$disk --table "0 $S delay /dev/$disk 0 8 /dev/$disk 0 8 /dev/$disk 0 20" \
        || { r $mode-dm FAIL; return; }
    # No udev here, so no /dev/mapper node: devtmpfs names it dm-N.
    slow=
    for d in /sys/block/dm-*; do
        [ "$(cat $d/dm/name)" = slow$disk ] && slow=/dev/$(basename $d)
    done
    [ -b "$slow" ] || { r $mode-dm "FAIL (no node for slow$disk)"; return; }
    mkdir -p /run/sb$disk
    cat > /run/sb$disk.toml <<EOT
[management]
api_token = "t"
listen_addr = "127.0.0.1:$port"
data_dir = "/run/sb$disk"
node_name = "ci-ublk"
discovery_disabled = true
EOT
    if [ "$mode" = serial ]; then export STORMBLOCK_UBLK_SERIAL=1; else unset STORMBLOCK_UBLK_SERIAL; fi
    RUST_LOG=stormblock=warn stormblock --config /run/sb$disk.toml --data-dir /run/sb$disk \
        --no-iscsi > /run/engine-$mode.log 2>&1 &
    pid=$!
    api() { curl -sf -m 120 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
    for i in $(seq 1 100); do api http://127.0.0.1:$port/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
    out=$(curl -s -m 120 -w ' HTTP%{http_code}' -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
        -X POST http://127.0.0.1:$port/api/v1/slabs \
        -d "{\"device_path\":\"$slow\",\"role\":\"data\"}")
    case "$out" in *HTTP2??) ;; *) r $mode-slab "FAIL ($out)"; sed 's/^/LOG /' /run/engine-$mode.log | tail -8; kill $pid; return ;; esac
    id=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d '{"name":"qd","size":"256M"}' \
        | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    dev=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$id/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    [ -b "$dev" ] || { r $mode-attach "FAIL ($id '$dev')"; sed 's/^/LOG /' /run/engine-$mode.log | tail -8; kill $pid; return; }
    echo "GUEST $mode: volume $id on $dev over $slow (slow$disk)"

    # Allocate the first 64 MiB so reads reach the disk.
    dd if=/dev/urandom of=/tmp/fill bs=1M count=64 2>/dev/null
    dd if=/tmp/fill of=$dev bs=1M count=64 oflag=direct 2>/dev/null
    echo 3 > /proc/sys/vm/drop_caches

    t0=$(now)
    for j in $(seq 1 16); do
        ( for k in $(seq 1 32); do dd if=$dev of=/dev/null bs=4096 count=1 skip=$(rnd) iflag=direct 2>/dev/null; done ) &
    done
    wait
    t1=$(now)
    echo "TIME $mode parallel-reads $(ms $t0 $t1)"

    ( while [ ! -e /tmp/stop ]; do
        dd if=/dev/urandom of=$dev bs=4096 count=1 seek=$(rnd) oflag=direct conv=fsync 2>/dev/null
      done ) &
    w=$!
    sleep 1
    t0=$(now)
    for k in $(seq 1 64); do dd if=$dev of=/dev/null bs=4096 count=1 skip=$(rnd) iflag=direct 2>/dev/null; done
    t1=$(now)
    touch /tmp/stop; wait $w; rm -f /tmp/stop
    echo "TIME $mode reads-under-fsync $(ms $t0 $t1)"

    dd if=/dev/urandom of=/tmp/pat bs=1M count=16 2>/dev/null
    for j in 0 1 2 3 4 5 6 7; do
        dd if=/tmp/pat of=$dev bs=64k skip=$((j * 32)) seek=$((2048 + j * 32)) count=32 oflag=direct 2>/dev/null &
    done
    wait
    dd if=$dev of=/tmp/back bs=64k skip=2048 count=256 iflag=direct 2>/dev/null
    cmp -s /tmp/pat /tmp/back && r $mode-round-trip PASS || r $mode-round-trip FAIL

    kill -TERM $pid; wait $pid
    grep -E "ERROR|panicked" /run/engine-$mode.log | head -5
}

run serial vda 9091
run concurrent vdb 9092
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
truncate -s 1G "$W/a.img" "$W/b.img"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 600 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 2048 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -drive file="$W/a.img",if=virtio,format=raw \
    -drive file="$W/b.img",if=virtio,format=raw > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|TIME|LOG)|ublk|ERROR|panick' | tail -60

t() { tr -d '\r' < "$W/guest.log" | awk -v m="$1" -v w="$2" '$1=="TIME" && $2==m && $3==w {print $4}'; }
SP=$(t serial parallel-reads); CP=$(t concurrent parallel-reads)
SF=$(t serial reads-under-fsync); CF=$(t concurrent reads-under-fsync)
echo "parallel reads:    serial ${SP:-?} ms, concurrent ${CP:-?} ms"
echo "reads under fsync: serial ${SF:-?} ms, concurrent ${CF:-?} ms"
for m in serial concurrent; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $m-round-trip PASS" || fail "$m round trip"
done
if [ -n "$SP" ] && [ -n "$CP" ] && [ "$CP" -gt 0 ]; then
    [ $((SP)) -ge $((CP * 3)) ] || fail "served at once is not 3x faster on parallel reads ($SP vs $CP ms)"
else
    fail "no timings"
fi
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
