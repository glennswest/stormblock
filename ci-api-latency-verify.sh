#!/usr/bin/env bash
# ci-api-latency-verify.sh — where the engine's slow answers go (#521).
#
# The engine as root in a QEMU guest on one disk QEMU throttles to one
# actuator's budget (THROTTLE_BPS, 150 MB/s; THROTTLE_IOPS, 120 requests a
# second), holding GOLDENS sealed ext4 goldens written at once (so their slots
# interleave in the slot table) and CLONES clones of them, with DEVICES of those
# attached over ublk, as a node carries. Then GET /volumes, GET /slabs, a clone,
# a delete and an fsck: warm; just after the engine restarted (its slot-table
# cache cold, as after a reboot); and warm under a writer that fsyncs. Each
# call's client time is printed, then the engine's own `api:` line with its
# steps. Prints the numbers; with MAX_MS set, each median must be under it.
#
# Run through sc-build: sc-build 'bash ci-api-latency-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-apilat.XXXXXX")
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

say "guest initramfs"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,mnt,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount umount insmod ip sleep cat echo ls grep dd poweroff dmesg head tail wc sed \
         awk cut tr kill seq mkdir rm timeout sort sync touch cp find ls; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$CURL"; do
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
cat > "$I/init" <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up
for w in $(cat /proc/cmdline); do case $w in lat.*=*) k=${w#lat.}; eval "P_${k%%=*}=${k#*=}";; esac; done
port=9590
B=http://127.0.0.1:$port/api/v1
api() { curl -sf -m 600 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
idof() { grep -o '"\(volume_\)\{0,1\}id":"[0-9a-f-]*"' | head -1 | cut -d'"' -f4; }
hint() { sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p'; }
now() { cut -d' ' -f1 /proc/uptime; }
ms() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%d", (b-a)*1000}'; }
mkdir -p /run/sb
cat > /run/sb.toml <<EOT
[[drives]]
path = "/dev/vda"

[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:$port"
data_dir = "/run/sb"
node_name = "ci-apilat"
discovery_disabled = true
EOT
start() {
    RUST_LOG=stormblock=info stormblock --config /run/sb.toml --data-dir /run/sb --no-iscsi >> /run/engine.log 2>&1 &
    EP=$!
    for i in $(seq 1 600); do api $B/health >/dev/null 2>&1 && return; sleep 0.2; done
    echo "RESULT start FAIL"; tail -5 /run/engine.log; poweroff -f
}
stormblock slab format /dev/vda --role data >/dev/null 2>&1
start
tmpl=$(api -X POST $B/fstemplates -d '{"name":"blank","size":"1G"}' | sed -n 's/.*"sealed_volume_id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
[ -n "$tmpl" ] || { echo "RESULT setup FAIL (template)"; tail -5 /run/engine.log; poweroff -f; }
# Goldens written at once: their slots interleave.
G=""
for n in $(seq 1 $P_goldens); do
    g=$(api -X POST $B/volumes/$tmpl/clone -d "{\"name\":\"gold$n\"}" | idof)
    d=$(api -X POST $B/volumes/$g/attach -d '{"transport":"ublk"}' | hint)
    mkdir -p /mnt/g$n; mount -t ext4 "$d" /mnt/g$n || { echo "RESULT setup FAIL (mount $d)"; poweroff -f; }
    G="$G $g"
done
for n in $(seq 1 $P_goldens); do ( for k in 1 2 3 4; do cat /bin/stormblock > /mnt/g$n/f$k; done; sync ) & done
wait_writers() { for j in $(jobs -p); do [ "$j" = "$EP" ] || wait "$j"; done; }
wait_writers
n=0
for g in $G; do
    n=$((n+1)); umount /mnt/g$n
    api -X DELETE $B/volumes/$g/attach >/dev/null
    api -X POST $B/volumes/$g/seal -d '{"force":true}' >/dev/null || { echo "RESULT setup FAIL (seal)"; poweroff -f; }
done
g1=${G%% *}; g1=$(echo $G | cut -d' ' -f1)
i=0; DEVN=0
while [ $i -lt $P_clones ]; do
    i=$((i+1)); s=$(echo $G | cut -d' ' -f$(( (i % P_goldens) + 1 )))
    c=$(api -X POST $B/volumes/$s/clone -d "{\"name\":\"svc$i\"}" | idof)
    [ -n "$c" ] || { echo "RESULT setup FAIL (clone $i)"; tail -5 /run/engine.log; poweroff -f; }
    if [ $DEVN -lt $P_devices ]; then
        api -X POST $B/volumes/$c/attach -d '{"transport":"ublk"}' >/dev/null; DEVN=$((DEVN+1))
    fi
done
lc=$(api -X POST $B/volumes/$tmpl/clone -d '{"name":"load"}' | idof)
ld=$(api -X POST $B/volumes/$lc/attach -d '{"transport":"ublk"}' | hint)
mkdir -p /mnt/load; mount -t ext4 "$ld" /mnt/load
sync
echo "GUEST $(api $B/volumes | grep -o '"id":"' | wc -l) volumes"

call() { # phase what curl-args...
    p=$1; w=$2; shift 2
    t0=$(now); api "$@" > /tmp/out; t1=$(now)
    echo "CALL $p $w $(ms $t0 $t1)"
}
round() { # phase
    for r in 1 2 3 4 5; do
        call $1 list $B/volumes
        call $1 slabs $B/slabs
    done
    for r in 1 2 3 4 5; do
        i0=$(ios); t0=$(now); c=$(api -X POST $B/volumes/$g1/clone -d "{\"name\":\"p-$1-$r\"}" | idof); t1=$(now); i1=$(ios)
        echo "CALL $1 clone $(ms $t0 $t1)"
        echo "IOS $1 clone $(diffios "$i0" "$i1")"
        t0=$(now); api -X POST $B/volumes/$c/attach -d '{"transport":"ublk"}' >/dev/null; t1=$(now)
        echo "CALL $1 attach $(ms $t0 $t1)"
        api -X DELETE $B/volumes/$c/attach >/dev/null
        if [ $r = 5 ]; then call $1 fsck -X POST $B/volumes/$c/fsck; fi
        i0=$(ios); t0=$(now); api -X DELETE $B/volumes/$c >/dev/null; t1=$(now); i1=$(ios)
        echo "CALL $1 delete $(ms $t0 $t1)"
        echo "IOS $1 delete $(diffios "$i0" "$i1")"
    done
}
# The disk's reads, writes and flushes a call cost (/sys/block/vda/stat).
ios() { awk '{print $1, $5, $15}' /sys/block/vda/stat; }
diffios() { # before after
    echo "$1 $2" | awk '{print "reads", $4-$1, "writes", $5-$2, "flushes", $6-$3}'
}
mark() { wc -l < /run/engine.log; }
lines() { # phase from-line
    tail -n +$2 /run/engine.log | sed 's/\x1b\[[0-9;]*m//g' \
      | grep -E 'api: (POST /api/v1/volumes/[0-9a-f-]+/(clone|fsck)|DELETE /api/v1/volumes/[0-9a-f-]+|GET /api/v1/(volumes|slabs)) ' \
      | sed -E 's#.*api: ##; s#/api/v1/volumes/[0-9a-f-]+#…#; s# caller.*##' | sed "s/^/ENGINE $1 /"
}
m=$(mark); round warm; lines warm $m

umount /mnt/load
kill $EP; for i in $(seq 1 300); do kill -0 $EP 2>/dev/null || break; sleep 0.1; done
start
ld=$(api $B/volumes | tr '{' '\n' | grep '"name":"load"' | head -1 >/dev/null; api -X POST $B/volumes/$lc/attach -d '{"transport":"ublk"}' | hint)
mount -t ext4 "$ld" /mnt/load
m=$(mark); round cold; lines cold $m

rm -f /tmp/stop
( while [ ! -e /tmp/stop ]; do dd if=/dev/zero of=/mnt/load/f bs=1M count=16 conv=fsync 2>/dev/null; done ) & LP=$!
sleep 3
m=$(mark); round load; lines load $m
touch /tmp/stop; wait $LP
grep -E "ERROR" /run/engine.log | sed 's/\x1b\[[0-9;]*m//g' | tail -5 | sed 's/^/LOG /'
echo "RESULT done PASS"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
truncate -s 40G "$W/a.img"

BPS=${THROTTLE_BPS:-150000000}
IOPS=${THROTTLE_IOPS:-120}
say "boot $KERNEL in QEMU (one disk: $BPS B/s, $IOPS requests/s)"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 2400 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 4096 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4 lat.goldens=${GOLDENS:-6} lat.clones=${CLONES:-90} lat.devices=${DEVICES:-85}" \
    -drive file="$W/a.img",if=virtio,format=raw,cache=none,aio=threads,throttling.bps-total=$BPS,throttling.iops-total=$IOPS \
    > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|CALL|IOS|ENGINE|LOG)|panick'
tr -d '\r' < "$W/guest.log" | grep -q "^RESULT done PASS" || { tail -30 "$W/guest.log"; fail "the guest did not finish"; }
for phase in warm cold load; do
    for what in list slabs clone attach delete fsck; do
        med=$(tr -d '\r' < "$W/guest.log" | awk -v p=$phase -v w=$what '$1=="CALL" && $2==p && $3==w {print $4}' | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')
        echo "MEDIAN $phase $what ${med:-?} ms"
        if [ -n "${MAX_MS:-}" ] && [ "$what" != fsck ] && [ "${med:-0}" -gt "$MAX_MS" ]; then fail "$phase $what ${med} ms is over ${MAX_MS} ms"; fi
    done
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
