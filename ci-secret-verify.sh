#!/usr/bin/env bash
# ci-secret-verify.sh — a secret volume on a real kernel (#74). Unprivileged
# on a build VM: the engine runs as root inside a QEMU guest, where ublk_drv
# is its own, over a slab on the guest's own disk.
#
# A secret volume (stormcert-data's class): attached as a local ublk device
# (written and read back through it), a second attach while that one is held
# refused (one consumer), a clone and an NVMe/TCP attach refused; detached,
# deleted, and its bytes gone from the disk under it although this node
# erases nothing by default.
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its
# ublk_drv, a static busybox and curl. Run through sc-build:
#   sc-build 'sh ci-secret-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-secret.XXXXXX")
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
for a in sh mount insmod ip sleep cat echo ls grep dd cmp poweroff head tail sed awk cut tr seq mkdir rm timeout wc sync printf; do
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
A=http://127.0.0.1:9091/api/v1
api() { curl -s -m 60 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
code() { curl -s -o /tmp/body -w '%{http_code}' -m 60 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
mkdir -p /run/a
cat > /run/a.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:9091"
data_dir = "/run/a"
node_name = "a"
discovery_disabled = true

[erase]
default = "none"
EOT
RUST_LOG=stormblock=info stormblock --config /run/a.toml --data-dir /run/a --no-iscsi > /run/a.log 2>&1 &
for i in $(seq 1 100); do api $A/health >/dev/null 2>&1 && break; sleep 0.2; done
api -X POST $A/slabs -d '{"device_path":"/dev/vda","role":"data"}' >/dev/null
id=$(api -X POST $A/volumes -d '{"name":"stormcert-data","size":"64M","secret":true}' | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
echo "GUEST secret volume $id"
at=$(api -X POST $A/volumes/$id/attach -d '{}')
dev=$(echo "$at" | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
[ -b "$dev" ] && r local-ublk PASS || r local-ublk "FAIL ($at)"
# What stormcert would write: a key, recognisable on the disk below.
i=0; : > /tmp/key; while [ $i -lt 4096 ]; do printf 'NODE-CA-PRIVATE-KEY-74' >> /tmp/key; i=$((i+1)); done
dd if=/tmp/key of=$dev bs=4096 conv=fsync,notrunc 2>/dev/null
dd if=$dev of=/tmp/back bs=4096 count=$(( $(wc -c < /tmp/key) / 4096 )) iflag=direct 2>/dev/null
head -c $(wc -c < /tmp/back) /tmp/key | cmp -s - /tmp/back && r reads-back PASS || r reads-back FAIL
[ "$(code -X POST $A/volumes/$id/attach -d '{}')" = 409 ] && grep -q "one consumer" /tmp/body && r one-consumer PASS || r one-consumer "FAIL ($(cat /tmp/body))"
[ "$(code -X POST $A/volumes/$id/clone -d '{"name":"copy"}')" = 409 ] && r clone-refused PASS || r clone-refused "FAIL ($(cat /tmp/body))"
[ "$(code -X POST $A/volumes/$id/attach -d '{"transport":"nvme_tcp"}')" = 409 ] && r nvme-refused PASS || r nvme-refused "FAIL ($(cat /tmp/body))"
grep -c 'NODE-CA-PRIVATE-KEY-74' /dev/vda >/dev/null 2>&1 && r key-on-disk PASS || r key-on-disk "FAIL (the write did not reach the slab)"
api -X DELETE $A/volumes/$id/attach >/dev/null
sleep 1
[ -b "$dev" ] && r detached "FAIL ($dev still there)" || r detached PASS
[ "$(code -X DELETE $A/volumes/$id)" -lt 300 ] && r deleted PASS || r deleted "FAIL ($(cat /tmp/body))"
gone=FAIL
for i in $(seq 1 60); do
    sync; echo 3 > /proc/sys/vm/drop_caches
    if ! grep -q 'NODE-CA-PRIVATE-KEY-74' /dev/vda 2>/dev/null; then gone=PASS; break; fi
    sleep 1
done
r erased "$gone"
grep -E "erase|secret" /run/a.log | sed 's/^/LOG A: /' | tail -6
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
    -drive file="$W/a.img",if=virtio,format=raw,cache=none > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG)|panick'

for m in local-ublk reads-back one-consumer clone-refused nvme-refused key-on-disk detached deleted erased; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
