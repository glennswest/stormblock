#!/usr/bin/env bash
# ci-boot-pagein-verify.sh — what a boot's page-in costs (#484).
#
# On a node's boot every unit pages its binary in from its own ublk root
# volume, on one disk, while the others do the same and something writes and
# fsyncs: 70-124 ms a major fault, against 3.5 ms quiet. Here: the engine as
# root in a QEMU guest on one disk QEMU throttles to one actuator's budget
# (THROTTLE_BPS, 150 MB/s; THROTTLE_IOPS, 120 requests a second). Service
# volumes are ext4 clones holding a real binary, over ublk.
#   EXEC    a real exec of that binary after a dropped cache: wall time, faults
#   DIRECT  O_DIRECT reads of a whole-written volume's ublk device, then of the
#           disk under it: what the engine adds to a read under the same load
#   PRE     /init's own boot read-ahead block, extracted, then the exec under load
# Each quiet and under boot load (LOAD_READERS units reading their volumes, one
# claim written and fsynced), for each read_ahead_kb in RA_LIST, and with the
# binary read ahead of the load (PRE). A PRE exec with a major fault fails.
#
# Prints the numbers; passes when every mode ran. Run through sc-build:
#   sc-build 'bash ci-boot-pagein-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-pagein.XXXXXX")
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX" "$CURL"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked --example pagein_probe 2>&1 | tail -2
cargo build --release --locked 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
PROBE="${CARGO_TARGET_DIR:-$ROOT/target}/release/examples/pagein_probe"
[ -x "$BIN" ] && [ -x "$PROBE" ] || { echo "FAIL: no binary"; exit 1; }

say "guest initramfs"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,mnt,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount umount insmod ip sleep cat echo ls grep dd poweroff dmesg head tail wc sed \
         awk cut tr kill seq mkdir rm timeout sort sync touch cp find ls; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$PROBE" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
for m in ublk_drv ext4; do
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
sed -n '/# --- BEGIN boot read-ahead/,/# --- END boot read-ahead/p' "$ROOT/scripts/build-stormblock-initramfs.sh" > "$I/bin/readahead.sh"
[ -s "$I/bin/readahead.sh" ] || { echo "FAIL: no read-ahead block"; exit 1; }
cat > "$I/init" <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up
for w in $(cat /proc/cmdline); do case $w in pagein.*=*) k=${w#pagein.}; eval "P_${k%%=*}=${k#*=}";; esac; done
port=9590
api() { curl -sf -m 300 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
idof() { grep -o '"\(volume_\)\{0,1\}id":"[0-9a-f-]*"' | head -1 | cut -d'"' -f4; }
mkdir -p /run/sb
cat > /run/sb.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:$port"
data_dir = "/run/sb"
node_name = "ci-pagein"
discovery_disabled = true
EOT
RUST_LOG=stormblock=warn stormblock --config /run/sb.toml --data-dir /run/sb --no-iscsi > /run/engine.log 2>&1 &
for i in $(seq 1 100); do api http://127.0.0.1:$port/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
api -X POST http://127.0.0.1:$port/api/v1/slabs -d '{"device_path":"/dev/vda","role":"data"}' >/dev/null \
    || { echo "RESULT setup FAIL (slab)"; tail -5 /run/engine.log; poweroff -f; }
tmpl=$(api -X POST http://127.0.0.1:$port/api/v1/fstemplates -d '{"name":"svc","size":"256M"}' \
    | sed -n 's/.*"sealed_volume_id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
[ -n "$tmpl" ] || { echo "RESULT setup FAIL (template)"; poweroff -f; }
clone() { # name -> device
    c=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$tmpl/clone -d "{\"name\":\"$1\"}" | idof)
    api -X POST http://127.0.0.1:$port/api/v1/volumes/$c/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p'
}
DEVS=""
for n in $(seq 0 $P_readers); do
    d=$(clone svc$n); mkdir -p /mnt/s$n
    mount -t ext4 "$d" /mnt/s$n || { echo "RESULT setup FAIL (mount $d)"; poweroff -f; }
    cp /bin/stormblock /mnt/s$n/bin
    [ $n = 0 ] && D0=$d || DEVS="$DEVS $d"
done
ld=$(clone load); mkdir -p /mnt/load
mount -t ext4 "$ld" /mnt/load || { echo "RESULT setup FAIL (load)"; poweroff -f; }
rawid=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d '{"name":"raw","size":"64M"}' | idof)
rawd=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$rawid/attach -d '{"transport":"ublk"}' \
    | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
dd if=/bin/stormblock of="$rawd" bs=1M count=64 oflag=direct 2>/dev/null
sync
echo "GUEST read_ahead_kb as attached: $(cat /sys/block/${D0##*/}/queue/read_ahead_kb)"
echo "GUEST binary $(wc -c < /mnt/s0/bin) bytes; rotational $(cat /sys/block/vda/queue/rotational); scheduler $(cat /sys/block/vda/queue/scheduler)"

ra() { for q in /sys/block/ublkb*/queue/read_ahead_kb; do echo "$1" > "$q"; done; }
drop() { sync; echo 3 > /proc/sys/vm/drop_caches; }
LP=""
load_on() {
    rm -f /tmp/stop
    for d in $DEVS; do pagein_probe hammer "$d" 3600 & LP="$LP $!"; done
    ( while [ ! -e /tmp/stop ]; do dd if=/dev/zero of=/mnt/load/f bs=1M count=16 conv=fsync 2>/dev/null; done ) & LP="$LP $!"
    sleep 2
}
load_off() { touch /tmp/stop; kill $LP 2>/dev/null; wait $LP 2>/dev/null; LP=""; }
execs() { # label
    for r in 1 2 3; do drop; echo "EXEC $1 $(pagein_probe exec /mnt/s0/bin --help)"; done
}
for k in $(echo $P_ra | tr , " "); do
    ra "$k"
    execs "quiet ra=$k"
    load_on
    execs "load ra=$k"
    load_off
done
ra 128
drop
load_on
echo "DIRECT load ublk $(pagein_probe direct "$rawd" 100)"
echo "DIRECT load disk $(pagein_probe direct /dev/vda 100)"
load_off
echo "DIRECT quiet ublk $(pagein_probe direct "$rawd" 100)"
echo "DIRECT quiet disk $(pagein_probe direct /dev/vda 100)"
ra 1024
mkdir -p /mnt/s0/p; umount /mnt/s0; mkdir -p /sysroot; mount -t ext4 "$D0" /sysroot
mkdir -p /sysroot/p/stormblock; cp /sysroot/bin /sysroot/p/stormblock/bin; sync
for r in 1 2 3; do
    drop
    echo "PRE block $(sh /bin/readahead.sh)"
    load_on
    echo "PRE then $(pagein_probe exec /sysroot/p/stormblock/bin --help)"
    load_off
done
grep -E "WARN|ERROR" /run/engine.log | sed 's/\x1b\[[0-9;]*m//g' | tail -5 | sed 's/^/LOG /'
echo "RESULT done PASS"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
truncate -s 8G "$W/a.img"

BPS=${THROTTLE_BPS:-150000000}
IOPS=${THROTTLE_IOPS:-120}
READERS=${LOAD_READERS:-8}
RA=${RA_LIST:-128,1024,4096}
say "boot $KERNEL in QEMU (one disk: $BPS B/s, $IOPS requests/s; $READERS readers)"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 2400 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 4096 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4 pagein.readers=$READERS pagein.ra=$RA" \
    -drive file="$W/a.img",if=virtio,format=raw,cache=none,aio=threads,throttling.bps-total=$BPS,throttling.iops-total=$IOPS \
    > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|EXEC|DIRECT|PRE|LOG)|panick'
tr -d '\r' < "$W/guest.log" | grep -q '^GUEST read_ahead_kb as attached: 1024' || fail "a device is not attached with 1 MiB of read-ahead"
[ "$(tr -d '\r' < "$W/guest.log" | grep -c '^PRE then .*major_faults 0 ')" = 3 ] || fail "a read-ahead binary still took major faults"
tr -d '\r' < "$W/guest.log" | grep -q "^RESULT done PASS" || { tail -30 "$W/guest.log"; fail "the guest did not finish"; }
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
