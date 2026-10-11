#!/usr/bin/env bash
# ci-vhost-user-blk-verify.sh — a QEMU guest's disk served by the engine
# over vhost-user-blk (#82): the guest's virtqueues handed to the engine
# through a Unix socket and shared memory, no kernel block device on the
# host. Unprivileged: the engine is a plain process, the guest a QEMU.
#
#   phase 1  attach rw over vhost-user-blk; the guest (the host's kernel in
#            QEMU, vhost-user-blk-pci, 2 queues) sees a virtio disk of the
#            volume's size at its 4096-byte block size, with discard and
#            write-zeroes, writes a 4 MiB pattern with fsync, drops its
#            caches and reads it back
#   phase 2  detach, attach again read-only (a new VMM, a fresh negotiation):
#            the guest's disk is read-only, holds phase 1's pattern, and a
#            write to it fails
#   engine   the volume is held while attached (delete 409), the socket goes
#            on detach
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its
# modules, a static busybox, curl, python3.
#   sc-build 'cargo build --locked && bash ci-vhost-user-blk-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
BIN=${STORMBLOCK_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/debug/stormblock}
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/vhost.XXXXXX")
MGMT=$((20000 + RANDOM % 20000))
TOKEN=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
SB_PID=""
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
die()  { echo "FAIL: $*"; exit 1; }
cleanup() {
    [ -n "$SB_PID" ] && kill "$SB_PID" 2>/dev/null && wait "$SB_PID" 2>/dev/null
    rm -rf "$W"
}
trap cleanup EXIT

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
for cmd in qemu-system-x86_64 curl python3 cpio; do
    command -v "$cmd" >/dev/null || { echo "SKIP: no $cmd"; exit 2; }
done
[ -x "$BIN" ] || die "no binary at $BIN (cargo build --locked first)"
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }

# ---------------------------------------------------------------- the engine
say "engine, API on 127.0.0.1:$MGMT"
mkdir -p "$W/data"
truncate -s 512M "$W/d1.img"
cat > "$W/stormblock.toml" <<EOF
[management]
api_token = "$TOKEN"
admin_token = "$TOKEN"
listen_addr = "127.0.0.1:$MGMT"
data_dir = "$W/data"
node_name = "ci-vhost"
discovery_disabled = true

[nvmeof]
export_drives = false
EOF
RUST_LOG=stormblock=info "$BIN" --config "$W/stormblock.toml" --device "$W/d1.img" --data-dir "$W/data" \
    --no-iscsi --nvmeof-addr "127.0.0.1:$((MGMT + 1))" > "$W/engine.log" 2>&1 &
SB_PID=$!
api() { curl -sf -m 120 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$@"; }
code() { curl -s -o /dev/null -w '%{http_code}' -m 60 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$@"; }
B="http://127.0.0.1:$MGMT/api/v1"
for _ in $(seq 1 150); do api "$B/health" >/dev/null 2>&1 && break; sleep 0.2; done
api "$B/health" >/dev/null || { tail -30 "$W/engine.log"; die "engine did not start"; }
r=$(curl -s -m 180 -w ' HTTP%{http_code}' -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -X POST "$B/slabs" -d "{\"device_path\":\"$W/d1.img\",\"role\":\"data\"}")
case "$r" in *HTTP2??) ;; *) die "slab: $r" ;; esac
VOL=$(api -X POST "$B/volumes" -d '{"name":"vm-disk","size":"64M"}' | j 'd["id"]') || die "volume"
echo "volume $VOL"

# ---------------------------------------------------------------- the guest
say "guest initramfs: busybox, virtio_blk"
head -c $((4 << 20)) /dev/urandom > "$W/pattern.bin"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,tmp,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod sleep cat echo ls dd cmp poweroff dmesg grep tail head md5sum sync sed tr cut wc; do
    ln -sf busybox "$I/bin/$a"
done
cp "$W/pattern.bin" "$I/pattern.bin"
: > "$I/lib/mods/order"
for m in virtio_pci virtio_blk; do
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
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
r() { echo "RESULT $1 $2"; }
PHASE=$(sed -n 's/.*vhostphase=\([a-z0-9]*\).*/\1/p' /proc/cmdline 2>/dev/null || true)
[ -n "$PHASE" ] || PHASE=$(cat /proc/cmdline | tr ' ' '\n' | grep '^vhostphase=' | cut -d= -f2)
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -b /dev/vda ] && break; sleep 1; done
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease), phase $PHASE"
if [ ! -b /dev/vda ]; then
    r disk-present FAIL
else
    r disk-present PASS
    q=/sys/block/vda/queue
    echo "GUEST vda: $(cat /sys/block/vda/size) sectors, lbs $(cat $q/logical_block_size), ro $(cat /sys/block/vda/ro), discard_max $(cat $q/discard_max_bytes), wz_max $(cat $q/write_zeroes_max_bytes), queues $(ls /sys/block/vda/mq | wc -l)"
    [ "$(cat /sys/block/vda/size)" = 131072 ] && r size-64m PASS || r size-64m "FAIL ($(cat /sys/block/vda/size))"
    [ "$(cat $q/logical_block_size)" = 4096 ] && r block-4096 PASS || r block-4096 "FAIL ($(cat $q/logical_block_size))"
    if [ "$PHASE" = rw ]; then
        [ "$(cat /sys/block/vda/ro)" = 0 ] && r writable PASS || r writable FAIL
        [ "$(cat $q/discard_max_bytes)" != 0 ] && r discard PASS || r discard FAIL
        [ "$(cat $q/write_zeroes_max_bytes)" != 0 ] && r write-zeroes PASS || r write-zeroes FAIL
        [ "$(ls /sys/block/vda/mq | wc -l)" = 2 ] && r two-queues PASS || r two-queues "FAIL ($(ls /sys/block/vda/mq | wc -l))"
        dd if=/pattern.bin of=/dev/vda bs=1M count=4 conv=fsync 2>/tmp/e && r write PASS || r write "FAIL ($(tail -1 /tmp/e))"
        sync; echo 3 > /proc/sys/vm/drop_caches
        dd if=/dev/vda of=/tmp/back bs=1M count=4 2>/dev/null
        cmp -s /tmp/back /pattern.bin && r read-back PASS || r read-back FAIL
    else
        [ "$(cat /sys/block/vda/ro)" = 1 ] && r read-only PASS || r read-only FAIL
        dd if=/dev/vda of=/tmp/back bs=1M count=4 2>/dev/null
        cmp -s /tmp/back /pattern.bin && r kept-across-attaches PASS || r kept-across-attaches FAIL
        if dd if=/pattern.bin of=/dev/vda bs=1M count=1 conv=fsync 2>/dev/null; then r write-refused FAIL; else r write-refused PASS; fi
    fi
fi
echo "GUEST dmesg:"; dmesg | grep -iE 'virtio|vda|blk' | tail -10
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
guest() {
    local phase=$1 sock=$2
    timeout 300 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 512 -smp 2 \
        -object memory-backend-memfd,id=mem,size=512M,share=on -numa node,memdev=mem \
        -chardev socket,id=vb,path="$sock" -device vhost-user-blk-pci,chardev=vb,num-queues=2 \
        -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
        -append "console=ttyS0 panic=-1 loglevel=4 vhostphase=$phase" > "$W/guest-$phase.log" 2>&1
    tr -d '\r' < "$W/guest-$phase.log" | grep -E '^(RESULT|GUEST)|vhost|virtio' | tail -30
    local n bad
    n=$(tr -d '\r' < "$W/guest-$phase.log" | grep -c '^RESULT ')
    bad=$(tr -d '\r' < "$W/guest-$phase.log" | grep '^RESULT ' | grep -vc ' PASS')
    [ "$n" -ge "$3" ] || fail "phase $phase: $n results, expected $3"
    [ "$bad" = 0 ] || fail "phase $phase: $bad guest check(s) failed"
}

say "phase 1: attach rw over vhost-user-blk"
A=$(api -X POST "$B/volumes/$VOL/attach" -d '{"transport":"vhost-user-blk"}') || die "attach"
SOCK=$(echo "$A" | j 'd["socket"]')
echo "socket $SOCK"
[ -S "$SOCK" ] && echo "ok: the socket is there" || fail "no socket at $SOCK"
[ "$(code -X DELETE "$B/volumes/$VOL")" = 409 ] && echo "ok: the volume is held while attached" || fail "delete while attached"
guest rw "$SOCK" 9
echo "engine: $(api "$B/volumes/$VOL/attach" | j 'd["info"]["requests"]') requests served"

say "phase 2: detach, attach again read-only"
api -X DELETE "$B/volumes/$VOL/attach" >/dev/null || fail "detach"
[ -e "$SOCK" ] && fail "the socket outlived the detach" || echo "ok: the socket went with the detach"
A=$(api -X POST "$B/volumes/$VOL/attach" -d '{"transport":"vhost-user-blk","mode":"ro"}') || die "attach ro"
guest ro "$(echo "$A" | j 'd["socket"]')" 6
api -X DELETE "$B/volumes/$VOL/attach" >/dev/null || fail "detach ro"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; grep -iE 'vhost|error|warn' "$W/engine.log" | tail -20; exit 1
