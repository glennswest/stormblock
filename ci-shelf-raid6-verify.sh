#!/usr/bin/env bash
# ci-shelf-raid6-verify.sh — the shelf test (#390) in QEMU, before the Dell's
# NetApp DS224C has its drives: 28 scsi_debug drives formatted 4096 + T10 PI
# type 1 (each its own store), the engine as root in the guest, and
# `examples/shelf_raid6_test` running the owner's plan against its API:
# two RAID-6 sets of 11 and 2 spares laid out; filled half and read back; one
# drive lost per set (rebuilt to the spare, timed); two lost in set A
# (readable), a third refused; two new spares (both rebuilt); a block flipped
# on a data member of set B found and put right by a scrub; a full scrub
# timed; both sets busy, and B while A rebuilds. Then the power cuts: the
# engine killed (-9) mid-write and mid-rebuild, restarted, the sets
# assembled again, consistent, the data right. The drives are RAM: the
# shape of the test, not the Dell's speed.
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules
# (ublk_drv, scsi_debug), a static busybox and curl.
# Run on a build VM:  sc-build 'bash ci-shelf-raid6-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-shelf.XXXXXX")
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX" "$CURL"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build the engine and the harness"
cargo build --release --locked --bin stormblock --example shelf_raid6_test 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
HARNESS="${CARGO_TARGET_DIR:-$ROOT/target}/release/examples/shelf_raid6_test"
[ -x "$BIN" ] && [ -x "$HARNESS" ] || { echo "FAIL: no binary"; exit 1; }

say "guest initramfs: busybox, stormblock, the harness, curl, ublk_drv, scsi_debug"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods,mnt}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod rmmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed \
         awk cut tr kill seq mkdir rm timeout pidof ps touch od mv umount sync basename sort; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$HARNESS" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
for m in ublk_drv scsi_debug; do
    modprobe -S "$KVER" --show-depends "$m" 2>/dev/null | awk '$1=="insmod"{print $2}'
done | awk '!seen[$0]++' | while read -r ko; do
    base=$(basename "$ko" | sed 's/\.xz$//; s/\.zst$//')
    case "$ko" in
        *.xz) xz -dc "$ko" > "$I/lib/mods/$base" ;;
        *.zst) zstd -dcq "$ko" > "$I/lib/mods/$base" ;;
        *) cp "$ko" "$I/lib/mods/$base" ;;
    esac
    [ "$base" = scsi_debug.ko ] || echo "$base" >> "$I/lib/mods/order"
done
[ -s "$I/lib/mods/scsi_debug.ko" ] || { echo "SKIP: no scsi_debug module for $KVER"; exit 2; }

cat > "$I/init" <<'GUEST'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
ip link set lo up
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
insmod /lib/mods/scsi_debug.ko add_host=28 per_host_store=1 num_tgts=1 dif=1 dix=1 \
    sector_size=4096 dev_size_mb=32 2>&1 | sed "s/^/LOG insmod scsi_debug: /"
for i in $(seq 1 100); do [ "$(ls -d /sys/block/sd* 2>/dev/null | wc -l)" -ge 28 ] && break; sleep 0.2; done
ALL=$(ls -d /sys/block/sd* | sed 's#.*/#/dev/#' | awk '{ print length($0), $0 }' | sort -n | cut -d' ' -f2 | tr '\n' ' ')
echo "GUEST drives: $ALL"
SHELF=""; EXTRA=""; n=0
for d in $ALL; do
    n=$((n + 1))
    if [ $n -le 24 ]; then SHELF="$SHELF,$d"; elif [ $n -le 28 ]; then EXTRA="$EXTRA,$d"; fi
done
SHELF=${SHELF#,}; EXTRA=${EXTRA#,}
LAST_EXTRA=${EXTRA##*,}
mkdir -p /run/sb
cat > /run/sb.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:9097"
data_dir = "/run/sb"
node_name = "ci-shelf"
discovery_disabled = true
EOT
start() {
    RUST_LOG=stormblock=info stormblock --config /run/sb.toml --data-dir /run/sb --no-iscsi >> /run/engine.log 2>&1 &
    pid=$!
    for i in $(seq 1 100); do curl -sf -m 2 http://127.0.0.1:9097/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
}
api() { curl -s -m 30 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
H="--api http://127.0.0.1:9097 --token t --state /run/state.json"
start
shelf_raid6_test run $H --drives $SHELF --extra $EXTRA --fill-pct 50 --rebuild-limit 240 2>&1 | sed 's/^/H /'

echo "GUEST step 8a: the engine killed mid-write"
shelf_raid6_test writeloop $H > /dev/null 2>&1 &
w=$!
sleep 3
kill -9 $pid; kill $w 2>/dev/null; wait $pid $w 2>/dev/null
start
shelf_raid6_test verify $H --drives $SHELF,$EXTRA --wait-clean 300 2>&1 | sed 's/^/H8A /'

echo "GUEST step 8b: the engine killed mid-rebuild"
A=$(grep -o '"array": *"[0-9a-f-]*"' /run/state.json | head -1 | sed 's/.*"\([0-9a-f-]*\)"$/\1/')
# Opened already (by `verify` after the first restart): its uuid by path.
api -X POST http://127.0.0.1:9097/api/v1/drives -d "{\"path\":\"$LAST_EXTRA\"}" > /dev/null
U=$(api http://127.0.0.1:9097/api/v1/drives | grep -o '"uuid":"[0-9a-f-]*","path":"'"$LAST_EXTRA"'"' \
    | sed 's/"uuid":"\([0-9a-f-]*\)".*/\1/' | head -1)
echo "LOG 8b: spare $LAST_EXTRA is drive '$U'"
api -X POST http://127.0.0.1:9097/api/v1/spares -d "{\"drive_uuid\":\"$U\",\"pool\":\"ds1\"}" > /dev/null
SLOT=$(api http://127.0.0.1:9097/api/v1/arrays/$A | grep -o '"index":[0-9]*,"uuid":"[0-9a-f-]*","state":"[A-Za-z]*"' \
    | grep -i active | head -1 | sed 's/"index":\([0-9]*\).*/\1/')
api -X POST http://127.0.0.1:9097/api/v1/arrays/$A/members/$SLOT/fail -d '{}' > /dev/null
sleep 1
echo "LOG 8b before the cut: $(api http://127.0.0.1:9097/api/v1/arrays/$A | grep -o '"status":{[^}]*')"
kill -9 $pid; wait $pid 2>/dev/null
start
shelf_raid6_test verify $H --drives $SHELF,$EXTRA --wait-clean 300 2>&1 | sed 's/^/H8B /'
grep -iE "panic|FATAL" /run/engine.log | head -5 | sed 's/^/LOG engine: /'
echo "GUEST done"
poweroff -f
GUEST
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 2400 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 3072 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" > "$W/guest.log" 2>&1
G=$(tr -d '\r' < "$W/guest.log")
echo "$G" | grep -E '^(H|H8A|H8B|GUEST|LOG) |ERROR|panick'
for m in drives-4k-pi shelf-laid-out fill-read-set0 fill-read-set1 one-lost-reads-set0 one-lost-reads-set1 \
         one-lost-rebuilt-set0 one-lost-rebuilt-set1 two-lost-reads third-refused third-refused-reads two-lost-rebuilt \
         corruption-found corruption-repaired corruption-clean-after corruption-data-right scrub-setA-clean \
         end-reads-set0 end-reads-set1; do
    echo "$G" | grep -q "^H RESULT $m PASS" || fail "$m"
done
for p in H8A H8B; do
    for m in after-cut-consistent-set0 after-cut-consistent-set1 after-cut-data-set0 after-cut-data-set1; do
        echo "$G" | grep -q "^$p RESULT $m PASS" || fail "$p $m"
    done
    echo "$G" | grep -q "^$p RESULT after-cut-clean" && fail "$p: a set not clean in time"
done
echo "$G" | grep "^H.* RESULT .* FAIL" | sed 's/^/  /'
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
