#!/usr/bin/env bash
# ci-ublk-flow-verify.sh — a ublk volume's flow before and after #409:
# queues and FUA, measured in a QEMU guest on a real kernel. Unprivileged
# on a build VM: the engine runs as root inside the guest.
#
# Two configurations, each on a fresh engine and a fresh volume:
#
#   before   STORMBLOCK_UBLK_QUEUES=1, STORMBLOCK_FUA_BY_FLUSH=1: one queue,
#            and every FUA write a write and the volume's flush (as before)
#   after    the defaults: one queue per CPU (the guest has 4), and a FUA
#            write into a slot already on the device written FUA at the drive
#
# and two kinds of disk (each configuration its own): fast (a guest virtio
# disk) and slow (one through dm-delay, 8 ms per read, write and flush: a
# 7200 rpm drive).
# On each, with `flow_bench` (examples/flow_bench.rs):
#
#   rand read/write   8 threads of random 4 KiB O_DIRECT I/O (fast disk)
#   commit fsync      4 writers, each rewriting its own record + fdatasync
#   commit dsync      the same with O_DSYNC writes (a FUA write each)
#
# Pass: the kernel sees the device as FUA-capable and with the queue count
# asked for; every commit record reads back as its writer left it; and, after
# #409, O_DSYNC commits on the slow disk are not slower than before. The
# numbers are printed for the issue; they are not otherwise asserted (a build
# VM's disk is not a node's).
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules
# (ublk_drv, dm-delay), a static busybox, dmsetup and curl.
#   sc-build 'sh ci-ublk-flow-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
DMSETUP=$(command -v dmsetup || true)
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-ublk-flow.XXXXXX")
FAILS=0
SECS=${FLOW_SECS:-5}

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX" "$DMSETUP" "$CURL"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked --bin stormblock --example flow_bench 2>&1 | tail -2
T="${CARGO_TARGET_DIR:-$ROOT/target}/release"
BIN="$T/stormblock"; BENCH="$T/examples/flow_bench"
[ -x "$BIN" ] && [ -x "$BENCH" ] || { echo "FAIL: no binaries"; exit 1; }

say "guest initramfs"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed \
         awk cut tr kill seq mkdir rm timeout pidof ps touch sync; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$BENCH" "$DMSETUP" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" 2>/dev/null | grep -o '/[^ ]*' | while read -r lib; do
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
cat > "$I/init" <<EOF
#!/bin/sh
SECS=$SECS
EOF
cat >> "$I/init" <<'EOF'
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
ip link set lo up
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease), $(grep -c ^processor /proc/cpuinfo) cpus"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
api() { curl -sf -m 120 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }

# The slow disks: vdc and vdd through dm-delay.
slow() {
    S=$(cat /sys/block/$1/size)
    dmsetup create --noudevsync slow$1 --table "0 $S delay /dev/$1 0 8 /dev/$1 0 8" || { r dm FAIL; poweroff -f; }
    for d in /sys/block/dm-*; do [ "$(cat $d/dm/name)" = slow$1 ] && echo /dev/${d##*/}; done
}
SLOWC=$(slow vdc); SLOWD=$(slow vdd)

# run <config> <disk> <port>: a fresh engine, slab, volume and device.
run() {
    cfg=$1; disk=$2; port=$3; tag=$cfg-$4
    mkdir -p /run/$tag
    cat > /run/$tag.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:$port"
data_dir = "/run/$tag"
node_name = "ci-flow"
discovery_disabled = true
EOT
    if [ "$cfg" = before ]; then
        export STORMBLOCK_UBLK_QUEUES=1 STORMBLOCK_FUA_BY_FLUSH=1; want_q=1
    else
        unset STORMBLOCK_UBLK_QUEUES STORMBLOCK_FUA_BY_FLUSH; want_q=4
    fi
    RUST_LOG=stormblock=warn stormblock --config /run/$tag.toml --data-dir /run/$tag --no-iscsi \
        > /run/engine-$tag.log 2>&1 &
    pid=$!
    for i in $(seq 1 100); do api http://127.0.0.1:$port/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
    api -X POST http://127.0.0.1:$port/api/v1/slabs -d "{\"device_path\":\"$disk\",\"role\":\"data\"}" > /dev/null \
        || { r $tag-slab FAIL; tail -5 /run/engine-$tag.log | sed 's/^/LOG /'; kill $pid; return; }
    id=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d '{"name":"flow","size":"256M"}' \
        | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    dev=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$id/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    [ -b "$dev" ] || { r $tag-attach FAIL; tail -5 /run/engine-$tag.log | sed 's/^/LOG /'; kill $pid; return; }
    b=${dev##*/}
    q=$(ls -d /sys/block/$b/mq/* 2>/dev/null | wc -l)
    fua=$(cat /sys/block/$b/queue/fua 2>/dev/null)
    echo "GUEST $tag: $dev queues=$q fua=$fua"
    [ "$q" = "$want_q" ] && r $tag-queues PASS || r $tag-queues "FAIL ($q, wanted $want_q)"
    [ "$fua" = 1 ] && r $tag-fua PASS || r $tag-fua "FAIL (queue/fua=$fua)"

    # Every slot the benchmarks touch taken and on the device first: what a
    # database's file is once it has run a while.
    dd if=/dev/urandom of=$dev bs=1M count=64 oflag=direct 2>/dev/null
    sync
    if [ "$4" = fast ]; then
        flow_bench rand $dev 8 $SECS read | sed "s/^/$tag /"
        flow_bench rand $dev 8 $SECS write | sed "s/^/$tag /"
    fi
    flow_bench commit $dev 4 $SECS fsync | sed "s/^/$tag /"
    flow_bench commit $dev 4 $SECS dsync | sed "s/^/$tag /"
    # Each writer's record is its own pattern (byte = writer + 1).
    ok=1
    for w in 0 1 2 3; do
        dd if=$dev of=/tmp/rec bs=4096 count=1 skip=$((w * 256)) iflag=direct 2>/dev/null
        want=$(printf "\\$(printf %o $((w + 1)))")
        n=$(tr -d "$want" < /tmp/rec | wc -c)
        [ "$n" = 0 ] || ok=0
    done
    [ $ok = 1 ] && r $tag-records PASS || r $tag-records FAIL
    api -X DELETE http://127.0.0.1:$port/api/v1/volumes/$id/attach > /dev/null
    kill $pid; wait $pid 2>/dev/null
}

run before /dev/vda 9091 fast
run after /dev/vdb 9092 fast
run before $SLOWC 9093 slow
run after $SLOWD 9094 slow
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
truncate -s 1G "$W/a.img" "$W/b.img" "$W/c.img" "$W/d.img"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 900 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 2048 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -drive file="$W/a.img",if=virtio,format=raw \
    -drive file="$W/b.img",if=virtio,format=raw \
    -drive file="$W/c.img",if=virtio,format=raw \
    -drive file="$W/d.img",if=virtio,format=raw > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG)|BENCH|ERROR|panick'

for t in before-fast after-fast before-slow after-slow; do
    for c in queues fua records; do
        tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $t-$c PASS" || fail "$t-$c"
    done
done
per() { tr -d '\r' < "$W/guest.log" | awk -v t="$1" -v k="$2" '$1==t && $4==k {sub("per_s=","",$7); print $7}'; }
B=$(per before-slow dsync); A=$(per after-slow dsync)
echo "slow disk, O_DSYNC commits/s: before ${B:-?}, after ${A:-?}"
if [ -n "$B" ] && [ -n "$A" ]; then
    [ "$A" -ge $((B * 9 / 10)) ] || fail "O_DSYNC commits slower after #409 ($A vs $B per s)"
else
    fail "no commit numbers"
fi
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
