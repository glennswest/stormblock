#!/usr/bin/env bash
# ci-pi-verify.sh — the engine on T10 PI drives (#389). Unprivileged on a
# build VM: the engine runs as root inside a QEMU guest, where ublk_drv and
# scsi_debug are its own.
#
# stormdrive formats shelf drives 4096 + PI type 1 where they can, 512 + PI
# type 1 elsewhere. The block layer's integrity profile (T10-DIF-TYPE1-CRC)
# then generates the guard and reference tags on every write and checks them
# on every read, between the kernel and the drive. For each of the two
# formats, a scsi_debug drive with DIF type 1 and DIX (`dif=1 dix=1
# sector_size=…`) and the engine on it:
#
#   format      the kernel says T10-DIF-TYPE1-CRC and the logical block
#   slab        a data slab formatted on it through the API
#   round trip  32 MiB written O_DIRECT through a ublk device, read back equal
#   ext4        an ext4 template cloned, attached, mounted, written, unmounted,
#               mounted again and read back
#   health      the slab's `logical_block` and `integrity` in health
#   no errors   no protection error in the kernel log
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules
# (ublk_drv, scsi_debug, ext4), a static busybox and curl.
# Run on a build VM:  sc-build 'bash ci-pi-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-pi.XXXXXX")
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

say "guest initramfs: busybox, stormblock, curl, ublk_drv, scsi_debug, ext4"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods,mnt}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod rmmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed \
         awk cut tr kill seq mkdir rm timeout pidof ps touch od mv umount sync basename; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
# scsi_debug last, loaded by the guest with its parameters: its dependencies
# go in the order list.
for m in ublk_drv ext4 scsi_debug; do
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

cat > "$I/init" <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
ip link set lo up
r() { echo "RESULT $1 $2"; }
api() { curl -sf -m 120 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }

pi() { # sector size, port
    ss=$1; port=$2; t=pi$ss
    insmod /lib/mods/scsi_debug.ko dif=1 dix=1 sector_size=$ss dev_size_mb=384 2>&1 | sed "s/^/LOG insmod scsi_debug: /"
    sd=
    for i in $(seq 1 50); do
        for b in /sys/block/sd*; do
            [ -e "$b" ] && grep -q scsi_debug "$b/device/model" 2>/dev/null && sd=${b##*/}
        done
        [ -n "$sd" ] && [ -b /dev/$sd ] && break
        sleep 0.2
    done
    [ -n "$sd" ] || { r $t-format "FAIL (no scsi_debug disk)"; return; }
    fmt=$(cat /sys/block/$sd/integrity/format 2>/dev/null); lbs=$(cat /sys/block/$sd/queue/logical_block_size)
    echo "GUEST $t: /dev/$sd integrity '$fmt', logical block $lbs"
    [ "$fmt" = T10-DIF-TYPE1-CRC ] && [ "$lbs" = $ss ] && r $t-format PASS || r $t-format "FAIL ($fmt, $lbs)"

    mkdir -p /run/$t
    cat > /run/$t.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:$port"
data_dir = "/run/$t"
node_name = "ci-pi"
discovery_disabled = true
EOT
    RUST_LOG=stormblock=info stormblock --config /run/$t.toml --data-dir /run/$t --no-iscsi > /run/engine-$t.log 2>&1 &
    pid=$!
    for i in $(seq 1 100); do api http://127.0.0.1:$port/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
    out=$(curl -s -m 120 -w ' HTTP%{http_code}' -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
        -X POST http://127.0.0.1:$port/api/v1/slabs -d "{\"device_path\":\"/dev/$sd\",\"role\":\"data\"}")
    case "$out" in *HTTP2??) r $t-slab PASS ;; *) r $t-slab "FAIL ($out)"; tail -8 /run/engine-$t.log | sed 's/^/LOG /'; kill $pid; return ;; esac

    id=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d '{"name":"pi","size":"128M"}' \
        | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    dev=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$id/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    if [ -b "$dev" ]; then
        dd if=/dev/urandom of=/tmp/pat bs=1M count=32 2>/dev/null
        dd if=/tmp/pat of=$dev bs=1M count=32 oflag=direct 2>/dev/null
        echo 3 > /proc/sys/vm/drop_caches
        dd if=$dev of=/tmp/back bs=1M count=32 iflag=direct 2>/dev/null
        cmp -s /tmp/pat /tmp/back && r $t-round-trip PASS || r $t-round-trip FAIL
        curl -s -m 30 -o /dev/null -H 'Authorization: Bearer t' -X DELETE http://127.0.0.1:$port/api/v1/volumes/$id/attach
    else
        r $t-round-trip "FAIL (no device: '$dev')"
    fi

    g=$(api -X POST http://127.0.0.1:$port/api/v1/fstemplates -d '{"name":"pi-root","size":"64M"}' \
        | sed -n 's/.*"sealed_volume_id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    c=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$g/clone -d '{"name":"pi-claim"}' \
        | grep -o '"\(volume_\)\{0,1\}id":"[0-9a-f-]*"' | head -1 | cut -d'"' -f4)
    d=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$c/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    if [ -b "$d" ] && mount -t ext4 "$d" /mnt; then
        dd if=/tmp/pat of=/mnt/f bs=1M count=8 2>/dev/null; sync; umount /mnt
        echo 3 > /proc/sys/vm/drop_caches
        mount -t ext4 "$d" /mnt && dd if=/mnt/f of=/tmp/f bs=1M 2>/dev/null; umount /mnt
        dd if=/tmp/pat of=/tmp/p8 bs=1M count=8 2>/dev/null
        cmp -s /tmp/f /tmp/p8 && r $t-ext4 PASS || r $t-ext4 FAIL
    else
        r $t-ext4 "FAIL (template '$g' clone '$c' device '$d')"
    fi

    # Health's slab report is cached for 10 s (`slab_report::TTL`).
    sleep 11
    h=$(curl -s -m 5 http://127.0.0.1:$port/api/v1/health)
    echo "LOG $t health slabs: $(echo "$h" | grep -o '"items":\[[^]]*\]')"
    case "$h" in *"\"logical_block\":$ss,\"integrity\":\"T10-DIF-TYPE1-CRC\""*) r $t-health PASS ;; *) r $t-health FAIL ;; esac

    kill -TERM $pid; wait $pid
    if dmesg | grep -iE "protection|guard tag|ref(erence)? tag|dif.*error|integrity.*error" | grep -v "Enabling DIF" | grep -qi "error"; then
        dmesg | grep -iE "protection|guard|tag|dif" | tail -5 | sed 's/^/LOG /'
        r $t-no-errors FAIL
    else
        r $t-no-errors PASS
    fi
    rmmod scsi_debug 2>&1 | sed 's/^/LOG rmmod: /'
}

pi 4096 9095
pi 512 9096
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 600 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 2048 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG)|ERROR|panick'
for t in pi4096 pi512; do
    for m in format slab round-trip ext4 health no-errors; do
        tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $t-$m PASS" || fail "$t-$m"
    done
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
