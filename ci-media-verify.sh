#!/usr/bin/env bash
# ci-media-verify.sh — imported media read by the Linux kernel (#110).
# Unprivileged: the kernel is a guest.
#
# An imported ISO or disk image was presented at 4096-byte blocks, so Linux
# could read none of it: isofs refuses a 4096 sector outright ("unsupported/
# invalid hardware sector size 4096"), and a GPT laid at 512 read at 4096 has
# its header at the wrong byte, so no partition appears. An import is now
# presented at the block its image was authored for, and so is every clone.
# This imports a real ISO (xorriso) and a real GPT disk image (sfdisk, a FAT
# partition) with no `lba` given, clones each, attaches the clones over
# NVMe/TCP to the host's kernel in QEMU and asks:
#
#   the engine   both imports answer lba 512, and so do their clones
#   Linux        both clones are 512-byte logical; the ISO's clone is as long
#                as the ISO (no tail lost); it mounts as iso9660 and the file
#                in it reads back byte for byte; the disk's clone shows its
#                partition, which mounts and reads back byte for byte; a plain
#                volume is still 4096; the ISO imported at `lba: 4096` is
#                refused by isofs, as every import was before (the control)
#
# Needs: cargo, qemu-system-x86_64, xorriso, sfdisk, mkfs.vfat + mtools,
# /boot/vmlinuz-$(uname -r) and its modules, a static busybox, nvme-cli, curl.
#   sc-build 'cargo build --locked && bash ci-media-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
NVME=$(command -v nvme || true)
ROOT=$(pwd)
BIN=${STORMBLOCK_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/debug/stormblock}
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-media.XXXXXX")
PORT=$((20000 + RANDOM % 20000))
MGMT=$((PORT + 1))
TOKEN=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
H1="nqn.2014-08.org.nvmexpress:uuid:22222222-3333-4444-5555-666666666602"
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

for need in "$KERNEL" "$BUSYBOX" "$NVME"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
for cmd in qemu-system-x86_64 xorriso sfdisk mkfs.vfat mcopy curl python3; do
    command -v "$cmd" >/dev/null || { echo "SKIP: no $cmd"; exit 2; }
done
[ -x "$BIN" ] || die "no binary at $BIN (cargo build --locked first)"
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }

# ---------------------------------------------------------------- the media
say "an ISO and a GPT disk image, as the world authors them"
head -c 300000 /dev/urandom > "$W/payload.bin"
mkdir -p "$W/isodir"
cp "$W/payload.bin" "$W/isodir/PAYLOAD.BIN"
xorriso -as mkisofs -quiet -V STORMMEDIA -o "$W/media.iso" "$W/isodir" || die "xorriso"
# A tail past the last 4 KiB, as most real ISOs have (#110 lost it): one
# more 2048-byte sector when xorriso padded to 4 KiB.
[ $(( $(stat -c %s "$W/media.iso") % 4096 )) = 0 ] && head -c 2048 /dev/zero >> "$W/media.iso"
ISO_LEN=$(stat -c %s "$W/media.iso")
echo "ISO $ISO_LEN bytes ($((ISO_LEN / 2048)) sectors of 2048; $((ISO_LEN % 4096)) past the last 4 KiB)"
truncate -s 64M "$W/disk.img"
printf 'label: gpt\nstart=2048, size=65536, type=linux\n' | sfdisk -q "$W/disk.img" || die "sfdisk"
truncate -s 32M "$W/part.img"
mkfs.vfat -S 512 -n STORMPART "$W/part.img" >/dev/null || die "mkfs.vfat"
mcopy -i "$W/part.img" "$W/payload.bin" ::/PAYLOAD.BIN || die "mcopy"
dd if="$W/part.img" of="$W/disk.img" bs=1M seek=1 conv=notrunc status=none

# ---------------------------------------------------------------- the engine
say "engine on 127.0.0.1:$PORT (NVMe/TCP) and :$MGMT (API)"
mkdir -p "$W/data"
truncate -s 1G "$W/d1.img"
cat > "$W/stormblock.toml" <<EOF
[management]
api_token = "$TOKEN"
admin_token = "$TOKEN"
listen_addr = "127.0.0.1:$MGMT"
data_dir = "$W/data"
node_name = "ci-media"
advertised_addr = "10.0.2.2"
discovery_disabled = true

[nvmeof]
export_drives = false
EOF
RUST_LOG=stormblock=info "$BIN" --config "$W/stormblock.toml" \
    --device "$W/d1.img" --data-dir "$W/data" --no-iscsi \
    --nvmeof-addr "127.0.0.1:$PORT" --nvmeof-nqn "nqn.2026-10.lo.storm:ci-media" \
    > "$W/engine.log" 2>&1 &
SB_PID=$!
api() { curl -sf -m 300 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$@"; }
B="http://127.0.0.1:$MGMT/api/v1"
for _ in $(seq 1 150); do api "$B/health" >/dev/null 2>&1 && break; sleep 0.2; done
api "$B/health" >/dev/null || { tail -30 "$W/engine.log"; die "engine did not start"; }
r=$(curl -s -m 180 -w ' HTTP%{http_code}' -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -X POST "$B/slabs" -d "{\"device_path\":\"$W/d1.img\",\"role\":\"system\"}")
case "$r" in *HTTP2??) ;; *) die "slab: $r" ;; esac

# import NAME FILE [EXTRA-JSON] -> prints "volume_id lba"
import() {
    local id st
    id=$(api -X POST "$B/volumes/import" -d "{\"name\":\"$1\",\"file\":\"$2\",\"format\":\"raw\"${3:-}}" | j 'd["id"]') \
        || die "import $1"
    for _ in $(seq 1 900); do
        st=$(api "$B/volumes/import/$id")
        case "$(echo "$st" | j 'd["state"]')" in
            Done|done) echo "$st" | j 'd["volume_id"] + " " + str(d.get("lba"))'; return 0 ;;
            Failed|failed) die "import $1: $(echo "$st" | j 'd.get("error")')" ;;
        esac
        sleep 0.2
    done
    die "import $1 did not finish"
}
clone() {
    local c
    c=$(api -X POST "$B/volumes/$1/clone" -d "{\"name\":\"$2\"}") || die "clone $2"
    [ "$(echo "$c" | j 'd["lba"]')" = "$3" ] && echo "ok: $2 is presented at $3" || fail "$2 is presented at $(echo "$c" | j 'd["lba"]'), not $3"
    echo "$c" | j 'd["id"]' > "$W/$2.id"
}

say "import with no lba given"
read -r ISO_G ISO_LBA <<<"$(import media.iso "$W/media.iso")"
read -r DISK_G DISK_LBA <<<"$(import disk.img "$W/disk.img")"
read -r CTRL_G CTRL_LBA <<<"$(import media4k.iso "$W/media.iso" ',"lba":4096')"
[ "$ISO_LBA" = 512 ] && echo "ok: the ISO imports at 512" || fail "the ISO imports at $ISO_LBA"
[ "$DISK_LBA" = 512 ] && echo "ok: the GPT disk imports at 512" || fail "the GPT disk imports at $DISK_LBA"
[ "$CTRL_LBA" = 4096 ] && echo "ok: lba 4096 asked, 4096 given (the control)" || fail "the control is $CTRL_LBA"
clone "$ISO_G" iso-clone 512
clone "$DISK_G" disk-clone 512
clone "$CTRL_G" iso4k-clone 4096
PLAIN=$(api -X POST "$B/volumes" -d '{"name":"plain","size":"64M"}' | j 'd["id"]') || die "plain volume"

attach() { api -X POST "$B/volumes/$1/attach" -d "{\"transport\":\"nvme-tcp\",\"host_nqn\":\"$H1\"}" || die "attach $1"; }
RI=$(attach "$(cat "$W/iso-clone.id")"); RD=$(attach "$(cat "$W/disk-clone.id")")
RC=$(attach "$(cat "$W/iso4k-clone.id")"); RP=$(attach "$PLAIN")
SUB=$(echo "$RI" | j 'd["nqn"]')
NS_I=$(echo "$RI" | j 'd["nsid"]'); NS_D=$(echo "$RD" | j 'd["nsid"]')
NS_C=$(echo "$RC" | j 'd["nsid"]'); NS_P=$(echo "$RP" | j 'd["nsid"]')
echo "subsystem $SUB: iso $NS_I, disk $NS_D, control $NS_C, plain $NS_P"

# ------------------------------------------------------------- Linux, over NVMe/TCP
say "guest initramfs: busybox, nvme-cli, nvme-tcp, isofs, vfat"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,mnt,etc/nvme,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount umount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed basename cut tr stat; do
    ln -sf busybox "$I/bin/$a"
done
cp "$NVME" "$I/bin/nvme"
ldd "$NVME" | grep -o '/[^ ]*' | while read -r lib; do mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"; done
cp "$W/payload.bin" "$I/payload.bin"
: > "$I/lib/mods/order"
for m in virtio_net nvme-tcp isofs vfat nls_cp437 nls_iso8859-1 nls_utf8; do
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
cat > "$I/env" <<EOF
PORT=$PORT
SUB=$SUB
NS_I=$NS_I
NS_D=$NS_D
NS_C=$NS_C
NS_P=$NS_P
H1=$H1
ISO_LEN=$ISO_LEN
EOF
cat > "$I/init" <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
. /env
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up; ip link set eth0 up
ip addr add 10.0.2.15/24 dev eth0; ip route add default via 10.0.2.2
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
if nvme connect -t tcp -a 10.0.2.2 -s "$PORT" -n "$SUB" --hostnqn "$H1" >/tmp/o 2>&1; then
    sleep 3
    dev_of() { for b in /sys/block/nvme*n*; do [ "$(cat $b/nsid 2>/dev/null)" = "$1" ] && basename $b && return; done; }
    i=$(dev_of "$NS_I"); d=$(dev_of "$NS_D"); c=$(dev_of "$NS_C"); p=$(dev_of "$NS_P")
    echo "GUEST iso /dev/$i, disk /dev/$d, control /dev/$c, plain /dev/$p"
    lbs() { cat /sys/block/$1/queue/logical_block_size; }
    [ "$(lbs $i)" = 512 ] && r iso-logical-512 PASS || r iso-logical-512 "FAIL ($(lbs $i))"
    [ "$(lbs $d)" = 512 ] && r disk-logical-512 PASS || r disk-logical-512 "FAIL ($(lbs $d))"
    [ "$(lbs $p)" = 4096 ] && r plain-stays-4096 PASS || r plain-stays-4096 "FAIL ($(lbs $p))"
    bytes=$(( $(cat /sys/block/$i/size) * 512 ))
    [ "$bytes" = "$ISO_LEN" ] && r iso-whole-length PASS || r iso-whole-length "FAIL ($bytes of $ISO_LEN)"
    if mount -t iso9660 -o ro /dev/$i /mnt 2>/tmp/m; then
        r iso-mounts PASS
        f=$(ls /mnt | grep -i payload | head -1)
        cmp -s "/mnt/$f" /payload.bin && r iso-file-reads-back PASS || r iso-file-reads-back "FAIL ($f)"
        umount /mnt
    else
        r iso-mounts "FAIL ($(cat /tmp/m))"
    fi
    [ -e /sys/block/$d/${d}p1 ] && r disk-partition-appears PASS || r disk-partition-appears FAIL
    if mount -t vfat -o ro /dev/${d}p1 /mnt 2>/tmp/m; then
        r disk-partition-mounts PASS
        cmp -s /mnt/PAYLOAD.BIN /payload.bin && r disk-file-reads-back PASS || r disk-file-reads-back FAIL
        umount /mnt
    else
        r disk-partition-mounts "FAIL ($(cat /tmp/m))"
    fi
    # The control: what every import was before. isofs must refuse it.
    if mount -t iso9660 -o ro /dev/$c /mnt 2>/tmp/m; then
        umount /mnt; r control-4096-refused "FAIL (mounted)"
    else
        r control-4096-refused PASS
    fi
    nvme disconnect -n "$SUB" >/dev/null 2>&1
else
    r connect "FAIL ($(head -2 /tmp/o))"
fi
echo "GUEST dmesg:"
dmesg | grep -iE 'isofs|iso9660|nvme|fat|sector size' | tail -20
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU, attach the clones over NVMe/TCP"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 300 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1024 -smp 2 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST)|isofs|ISOFS|sector size' | tail -40
results=$(tr -d '\r' < "$W/guest.log" | grep -c '^RESULT ')
bad=$(tr -d '\r' < "$W/guest.log" | grep '^RESULT ' | grep -vc ' PASS')
[ "$results" -ge 10 ] || fail "the guest reported $results results, expected 10"
[ "$bad" = 0 ] || fail "$bad guest check(s) failed"

if [ "$FAILS" = 0 ]; then
    echo "ALL PASS"
else
    echo "$FAILS FAILURE(S)"
    tail -20 "$W/engine.log"
    exit 1
fi
