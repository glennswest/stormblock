#!/usr/bin/env bash
# ci-stress-probe-verify.sh — what a claim's start costs while another claim
# is rewritten (#491). stormcentral's stress stage starts a container, a
# claim and a VM while a pod rewrites a 2 GiB file on a claim in a loop
# (`dd bs=4M … conv=fsync`): 3–4× slower on pve, 26.7× on the Dell's HDD.
#
# The engine runs as root in a QEMU guest on one disk that QEMU throttles
# (THROTTLE_BPS, 60 MB/s; THROTTLE_IOPS, 200 a second, counted per 64 KiB):
# a shared budget, so a small read waits behind big writes as on one HDD
# (dm-delay delays each I/O on its own and models no queue). A probe is what
# a claim's start asks of the engine: clone an ext4 template, attach it over
# ublk, mount it, write a file and fsync, unmount, detach, delete. Each step
# is timed, clean (3 runs) and under the load (3 runs): a 1 GiB claim
# mounted in the guest with `dd if=/dev/zero bs=4M count=128 conv=fsync` in
# a loop. The engine's own request times for the probe's calls are printed
# too.
#
# Pass: every probe completes. With MAX_RATIO set, the loaded probe's median
# total must be within MAX_RATIO of the clean one.
#
# Needs what ci-ublk-qd-verify.sh needs. Run through sc-build:
#   sc-build 'bash ci-stress-probe-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-stress.XXXXXX")
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
         awk cut tr kill seq mkdir rm timeout sort sync touch; do
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
port=9590
now() { cut -d' ' -f1 /proc/uptime; }
ms() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%d", (b-a)*1000}'; }
api() { curl -sf -m 300 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
idof() { grep -o '"\(volume_\)\{0,1\}id":"[0-9a-f-]*"' | head -1 | cut -d'"' -f4; }
mkdir -p /run/sb
cat > /run/sb.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:$port"
data_dir = "/run/sb"
node_name = "ci-stress"
discovery_disabled = true
EOT
RUST_LOG=stormblock=info stormblock --config /run/sb.toml --data-dir /run/sb --no-iscsi > /run/engine.log 2>&1 &
for i in $(seq 1 100); do api http://127.0.0.1:$port/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
api -X POST http://127.0.0.1:$port/api/v1/slabs -d '{"device_path":"/dev/vda","role":"data"}' >/dev/null \
    || { echo "RESULT setup FAIL (slab)"; tail -5 /run/engine.log; poweroff -f; }
probe_t=$(api -X POST http://127.0.0.1:$port/api/v1/fstemplates -d '{"name":"probe-root","size":"64M"}' \
    | sed -n 's/.*"sealed_volume_id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
load_t=$(api -X POST http://127.0.0.1:$port/api/v1/fstemplates -d '{"name":"load-claim","size":"1G"}' \
    | sed -n 's/.*"sealed_volume_id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
[ -n "$probe_t" ] && [ -n "$load_t" ] || { echo "RESULT setup FAIL (templates)"; poweroff -f; }
mkdir -p /mnt/p /mnt/load

# One claim's start: clone, attach, mount, write + fsync, then gone again.
probe() { # label n
    t0=$(now)
    c=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$probe_t/clone -d "{\"name\":\"probe-$1-$2\"}" | idof)
    t1=$(now)
    d=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$c/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    t2=$(now)
    mount -t ext4 "$d" /mnt/p || { echo "RESULT probe-$1 FAIL (mount $d)"; return; }
    t3=$(now)
    dd if=/dev/zero of=/mnt/p/f bs=4k count=16 conv=fsync 2>/dev/null
    t4=$(now)
    umount /mnt/p
    t5=$(now)
    api -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c/attach >/dev/null
    api -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c >/dev/null
    t6=$(now)
    echo "PROBE $1 $2 total $(ms $t0 $t6) clone $(ms $t0 $t1) attach $(ms $t1 $t2) mount $(ms $t2 $t3) write $(ms $t3 $t4) umount $(ms $t4 $t5) gone $(ms $t5 $t6)"
}

for n in 1 2 3; do probe clean $n; done

lc=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$load_t/clone -d '{"name":"load"}' | idof)
ld=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$lc/attach -d '{"transport":"ublk"}' \
    | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
mount -t ext4 "$ld" /mnt/load || { echo "RESULT load FAIL (mount $ld)"; poweroff -f; }
( while [ ! -e /tmp/stop ]; do dd if=/dev/zero of=/mnt/load/load bs=4M count=128 conv=fsync 2>/dev/null; echo x >> /tmp/passes; done ) &
lp=$!
sleep 8
mark=$(wc -l < /run/engine.log)
for n in 1 2 3; do probe load $n; done
touch /tmp/stop; wait $lp
echo "GUEST load passes: $(wc -l < /tmp/passes 2>/dev/null || echo 0) of 512 MiB"
# The engine's own times for the probe's calls under the load.
tail -n +$mark /run/engine.log | grep -E "api: (POST|DELETE) /api/v1/volumes/[0-9a-f-]+(/clone|/attach)? " \
    | sed 's/\x1b\[[0-9;]*m//g' | sed -n 's/.*api: \(\(POST\|DELETE\) [^ ]*\) \([0-9]*\) \([0-9]*ms\) lock-wait \([0-9]*ms\).*/\1 \3 \4 (lock-wait \5)/p' \
    | sed -E 's#/api/v1/volumes/[0-9a-f-]+#…#' | sed 's/^/ENGINE /'
grep -E "WARN|ERROR" /run/engine.log | sed 's/\x1b\[[0-9;]*m//g' | grep -v "unauthorized" | tail -8 | sed 's/^/LOG /'
umount /mnt/load
echo "RESULT done PASS"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
truncate -s 8G "$W/a.img"

BPS=${THROTTLE_BPS:-60000000}
IOPS=${THROTTLE_IOPS:-200}
say "boot $KERNEL in QEMU (one disk: $BPS B/s, $IOPS IOPS per 64 KiB)"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 1200 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 2048 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -drive file="$W/a.img",if=virtio,format=raw,cache=none,aio=threads,throttling.bps-total=$BPS,throttling.iops-total=$IOPS,throttling.iops-size=65536 \
    > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|PROBE|ENGINE|LOG)|panick'

med() { tr -d '\r' < "$W/guest.log" | awk -v l="$1" '$1=="PROBE" && $2==l {print $5}' | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'; }
C=$(med clean); L=$(med load)
echo "median probe: clean ${C:-?} ms, under load ${L:-?} ms"
tr -d '\r' < "$W/guest.log" | grep -q "^RESULT done PASS" || fail "the guest did not finish"
[ "$(tr -d '\r' < "$W/guest.log" | grep -c '^PROBE ')" = 6 ] || fail "not every probe completed"
if [ -n "${MAX_RATIO:-}" ] && [ -n "$C" ] && [ -n "$L" ]; then
    [ "$L" -le $((C * MAX_RATIO)) ] || fail "under load ${L} ms is more than ${MAX_RATIO}× clean ${C} ms"
fi
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
