#!/usr/bin/env bash
# ci-hba-reset-verify.sh — a slab's SCSI disk goes away under a write load
# and comes back (#391). Unprivileged on dev: the engine runs as root inside a
# QEMU guest whose slab disk sits on a virtio-scsi controller.
#
# What the Dell's mpt3sas does in a host reset — the disk answers nothing for
# seconds, I/O comes back failed (DID_RESET / DID_NO_CONNECT) — is made here
# with the SCSI device's state: `offline` (every I/O rejected with EIO, the
# device kept) and then `running`. A `host_reset` of the controller is tried
# too, where the driver has one (virtio-scsi has none: logged, not a failure).
#
# The engine serves a volume over ublk from a slab on /dev/sda, with a 20 s
# ride-through window (STORMBLOCK_TRANSPORT_WINDOW_SECS). A writer writes
# numbered 4 KiB blocks, each with an fsync, and records each one acknowledged.
#
#   ride      the disk offline for 8 s mid-load: the writer sees no error,
#             /api/v1/health lists the drive in `drives_unreachable` while it
#             is away and not once it is back, and every acknowledged block
#             reads back exactly
#   give up   the disk offline for 30 s (> the window): health says
#             `gave_up`, the engine logs the drive unreachable, the write in
#             flight fails (loudly, not forever); back online, a write+fsync
#             succeeds and the entry goes
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules
# (ublk_drv, virtio_scsi, sd_mod), a static busybox and curl. Run on dev:
#   sc-build 'sh ci-hba-reset-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-hba-reset.XXXXXX")
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

say "guest initramfs: busybox, stormblock, curl, ublk_drv, virtio_scsi, sd_mod"
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
for m in ublk_drv virtio_scsi sd_mod; do
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
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
ip link set lo up
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
for i in $(seq 1 50); do [ -b /dev/sda ] && break; sleep 0.2; done
[ -b /dev/sda ] || { r scsi-disk "FAIL (no /dev/sda)"; ls /sys/block | sed 's/^/LOG /'; poweroff -f; }
now() { cut -d' ' -f1 /proc/uptime; }
api() { curl -sf -m 120 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
health() { curl -s -m 5 http://127.0.0.1:9090/api/v1/health; }
STATE=/sys/block/sda/device/state

( sleep ${WATCHDOG:-400}
  echo "WATCHDOG fired"
  sed 's/^/LOG engine: /' /run/engine.log | tail -20
  for p in $(pidof stormblock); do
    for t in /proc/$p/task/*; do
      echo "STACK $(cat $t/comm) $(cat $t/wchan)"; sed 's/^/STACK   /' $t/stack | head -8
    done
  done
  poweroff -f ) &

mkdir -p /run/sb
cat > /run/sb.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:9090"
data_dir = "/run/sb"
node_name = "ci-hba"
discovery_disabled = true
EOT
STORMBLOCK_TRANSPORT_WINDOW_SECS=20 RUST_LOG=stormblock=info \
    stormblock --config /run/sb.toml --data-dir /run/sb --no-iscsi > /run/engine.log 2>&1 &
pid=$!
for i in $(seq 1 100); do api http://127.0.0.1:9090/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
out=$(curl -s -m 120 -w ' HTTP%{http_code}' -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
    -X POST http://127.0.0.1:9090/api/v1/slabs -d '{"device_path":"/dev/sda","role":"data"}')
case "$out" in *HTTP2??) ;; *) r slab "FAIL ($out)"; sed 's/^/LOG /' /run/engine.log | tail -8; poweroff -f ;; esac
id=$(api -X POST http://127.0.0.1:9090/api/v1/volumes -d '{"name":"root","size":"256M"}' \
    | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
dev=$(api -X POST http://127.0.0.1:9090/api/v1/volumes/$id/attach -d '{"transport":"ublk"}' \
    | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
[ -b "$dev" ] || { r attach "FAIL ($id '$dev')"; sed 's/^/LOG /' /run/engine.log | tail -8; poweroff -f; }
echo "GUEST volume $id on $dev, slab on /dev/sda ($(cat /sys/block/sda/device/vendor 2>/dev/null))"
dd if=/dev/urandom of=/tmp/pat bs=1M count=32 2>/dev/null

# Block i of the pattern to block i of the volume, each with an fsync; an
# acknowledged block is recorded, an error is recorded.
writer() {
    i=$1
    while [ ! -e /tmp/stop ]; do
        if dd if=/tmp/pat of=$dev bs=4096 skip=$i seek=$i count=1 oflag=direct conv=fsync,notrunc 2>/tmp/dd.err; then
            echo $i > /tmp/acked
        else
            echo "block $i at $(now): $(cat /tmp/dd.err)" >> /tmp/eio
        fi
        i=$((i + 1))
        [ $i -ge 8192 ] && i=0
    done
}

# --- ride: offline for 8 s mid-load ---
rm -f /tmp/stop /tmp/eio /tmp/acked
writer 0 &
w=$!
sleep 3
before=$(cat /tmp/acked 2>/dev/null)
echo offline > $STATE && echo "GUEST sda offline at $(now) (acked $before)"
sleep 5
h=$(health); echo "LOG health while away: $h"
case "$h" in *'"drives_unreachable"'*'/dev/sda'*'"gave_up":false'*) r ride-named PASS ;;
    *) r ride-named "FAIL (not in drives_unreachable)" ;; esac
sleep 3
echo running > $STATE && echo "GUEST sda running at $(now)"
for hr in /sys/class/scsi_host/host*/host_reset; do
    [ -e "$hr" ] || continue
    if echo 1 > $hr 2>/tmp/hr.err; then echo "GUEST host reset: $hr"; else
        echo "LOG host_reset not offered by this driver ($hr: $(cat /tmp/hr.err))"; fi
done
sleep 6
touch /tmp/stop; wait $w
n=$(cat /tmp/acked)
echo "GUEST writer acked through block $n ($before before the disk went)"
[ -n "$n" ] && [ "$n" -gt "${before:-0}" ] && r ride-progress PASS || r ride-progress "FAIL (no writes after)"
[ ! -s /tmp/eio ] && r ride-no-eio PASS || { r ride-no-eio FAIL; sed 's/^/LOG eio: /' /tmp/eio | head -5; }
count=$((n + 1))
dd if=$dev of=/tmp/back bs=4096 count=$count iflag=direct 2>/dev/null
dd if=/tmp/pat of=/tmp/want bs=4096 count=$count 2>/dev/null
cmp -s /tmp/want /tmp/back && r ride-data PASS || r ride-data "FAIL (acked blocks differ)"
h=$(health)
case "$h" in *drives_unreachable*) r ride-cleared "FAIL ($h)" ;; *) r ride-cleared PASS ;; esac
grep -E "riding through|answering again" /run/engine.log | sed 's/^/LOG /' | head -4

# --- give up: offline for 30 s, past the 20 s window ---
echo offline > $STATE && echo "GUEST sda offline at $(now), for 30 s"
t0=$(now)
dd if=/tmp/pat of=$dev bs=4096 skip=9000 seek=9000 count=1 oflag=direct conv=fsync,notrunc 2>/tmp/dd.err &
f=$!
sleep 26
h=$(health); echo "LOG health past the window: $h"
case "$h" in *'"drives_unreachable"'*'"gave_up":true'*) r giveup-named PASS ;;
    *) r giveup-named "FAIL (no gave_up)" ;; esac
grep -q "the drive is unreachable" /run/engine.log && r giveup-logged PASS || r giveup-logged FAIL
sleep 4
echo running > $STATE && echo "GUEST sda running at $(now)"
if wait $f; then echo "LOG the write during the outage completed: $(cat /tmp/dd.err)"; fi
sleep 2
if dd if=/tmp/pat of=$dev bs=4096 skip=9001 seek=9001 count=1 oflag=direct conv=fsync,notrunc 2>/dev/null; then
    r giveup-recovered PASS
else
    r giveup-recovered FAIL
fi
sleep 1
h=$(health)
case "$h" in *drives_unreachable*) r giveup-cleared "FAIL ($h)" ;; *) r giveup-cleared PASS ;; esac
kill -0 $pid 2>/dev/null && r engine-alive PASS || r engine-alive FAIL
kill -TERM $pid; wait $pid
grep -E "ERROR|panicked" /run/engine.log | sed 's/^/LOG /' | head -5
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
    -device virtio-scsi-pci,id=scsi0 \
    -drive file="$W/a.img",if=none,id=d0,format=raw \
    -device scsi-hd,drive=d0,bus=scsi0.0 > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG|STACK|WATCHDOG)|ERROR|panick'

for m in ride-named ride-progress ride-no-eio ride-data ride-cleared \
         giveup-named giveup-logged giveup-recovered giveup-cleared engine-alive; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
