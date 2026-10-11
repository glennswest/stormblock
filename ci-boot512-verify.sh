#!/usr/bin/env bash
# ci-boot512-verify.sh — boot media at 512-byte LBAs, checked by the Linux
# kernel and by UEFI firmware (#228). Unprivileged: both are guests.
#
# server1's AMI Aptio 4 and pve's OVMF could not boot a release: every volume
# was presented at 4096-byte LBAs, so firmware was handed a 4K GPT and a 4K
# FAT. A volume now carries its own LBA; a disk composed at 512 is presented
# at 512, and so is every clone of it (what a boot claim hands a machine).
# This composes one the way a release is composed and asks:
#
#   the engine    compose/disk {lba: 512} answers lba 512; its clone is 512;
#                 a plain volume beside it is still 4096
#   Linux         (the host's kernel in QEMU, nvme-cli over NVMe/TCP, as pve
#                 or an initramfs would attach it) sees 512-byte logical /
#                 4096-byte physical sectors, finds the GPT's two partitions,
#                 mounts the ESP — a 512-sector FAT — and reads stormuefi out
#                 of it byte for byte; the plain volume is 4096 to it
#   OVMF          boots the clone's bytes as a virtio disk with no block-size
#                 override (the pve shape): firmware finds the ESP, starts
#                 stormuefi, which picks the boot pallet and starts the kernel
#                 with the pallet's command line
#
# Needs: cargo, git (stormuefi), qemu-system-x86_64, OVMF, mkfs.vfat + mtools,
# /boot/vmlinuz-$(uname -r) and its modules, a static busybox, nvme-cli, curl.
#   sc-build 'cargo build --locked && bash ci-boot512-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
NVME=$(command -v nvme || true)
OVMF_CODE=${OVMF_CODE:-/usr/share/edk2/ovmf/OVMF_CODE.fd}
OVMF_VARS=${OVMF_VARS:-/usr/share/edk2/ovmf/OVMF_VARS.fd}
STORMUEFI=${STORMUEFI:-}
ROOT=$(pwd)
BIN=${STORMBLOCK_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/debug/stormblock}
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-boot512.XXXXXX")
PORT=$((20000 + RANDOM % 20000))
MGMT=$((PORT + 1))
TOKEN=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
H1="nqn.2014-08.org.nvmexpress:uuid:22222222-3333-4444-5555-666666666601"
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

for need in "$KERNEL" "$BUSYBOX" "$NVME" "$OVMF_CODE"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
for cmd in qemu-system-x86_64 mkfs.vfat mcopy curl python3; do
    command -v "$cmd" >/dev/null || { echo "SKIP: no $cmd"; exit 2; }
done
[ -x "$BIN" ] || die "no binary at $BIN (cargo build --locked first)"
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }

# ------------------------------------------------------------------ stormuefi
if [ -z "$STORMUEFI" ]; then
    say "stormuefi, built from its repo"
    # Outside this checkout: inside it, cargo takes stormuefi for a stray
    # member of stormblock's workspace.
    U=$(mktemp -d "$(dirname "$ROOT")/stormuefi.XXXXXX")
    git clone -q --depth 1 https://github.com/glennswest/stormuefi "$U/src" || die "clone stormuefi"
    (cd "$U/src" && CARGO_TARGET_DIR="$U/target" cargo build -q --release --target x86_64-unknown-uefi) \
        || die "build stormuefi"
    cp "$U/target/x86_64-unknown-uefi/release/stormuefi.efi" "$W/stormuefi.efi"
    rm -rf "$U"
    STORMUEFI="$W/stormuefi.efi"
fi
echo "stormuefi $(stat -c %s "$STORMUEFI") bytes; kernel $KERNEL"

# ---------------------------------------------------------------- the ESP
# 512-byte sectors: what firmware's FAT driver is dependable at, and what the
# disk it lands in is now presented at. FAT16, 64 MiB.
say "ESP: FAT16, 512-byte sectors, stormuefi as the default loader"
mkdir -p "$W/esp/EFI/BOOT"
cp "$STORMUEFI" "$W/esp/EFI/BOOT/BOOTX64.EFI"
mkfs.vfat -F 16 -S 512 -n EFI -C "$W/esp.img" 65536 >/dev/null || die "mkfs.vfat"
mcopy -i "$W/esp.img" -s "$W/esp/EFI" ::/ || die "mcopy"
EFI_LEN=$(stat -c %s "$STORMUEFI")

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
node_name = "ci-boot512"
advertised_addr = "10.0.2.2"
discovery_disabled = true

[nvmeof]
export_drives = false
EOF
RUST_LOG=stormblock=info "$BIN" --config "$W/stormblock.toml" \
    --device "$W/d1.img" --data-dir "$W/data" --no-iscsi \
    --nvmeof-addr "127.0.0.1:$PORT" --nvmeof-nqn "nqn.2026-09.lo.storm:ci-boot512" \
    > "$W/engine.log" 2>&1 &
SB_PID=$!
api() { curl -sf -m 300 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$@"; }
B="http://127.0.0.1:$MGMT/api/v1"
for _ in $(seq 1 150); do api "$B/health" >/dev/null 2>&1 && break; sleep 0.2; done
api "$B/health" >/dev/null || { tail -30 "$W/engine.log"; die "engine did not start"; }
r=$(curl -s -m 180 -w ' HTTP%{http_code}' -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -X POST "$B/slabs" -d "{\"device_path\":\"$W/d1.img\",\"role\":\"system\"}")
case "$r" in *HTTP2??) ;; *) die "slab: $r" ;; esac

import() {
    local id st
    id=$(api -X POST "$B/volumes/import" -d "{\"name\":\"$1\",\"file\":\"$2\",\"format\":\"raw\"}" | j 'd["id"]') \
        || die "import $1"
    for _ in $(seq 1 900); do
        st=$(api "$B/volumes/import/$id")
        case "$(echo "$st" | j 'd["state"]')" in
            Done|done) echo "$st" | j 'd["volume_id"]'; return 0 ;;
            Failed|failed) die "import $1: $(echo "$st" | j 'd.get("error")')" ;;
        esac
        sleep 0.2
    done
    die "import $1 did not finish"
}
say "goldens: kernel, ESP"
import kernel.golden "$KERNEL" >/dev/null
import esp512.golden "$W/esp.img" >/dev/null
KERNEL_LEN=$(stat -c %s "$KERNEL")

say "compose the boot pallet and the disk at 512"
P=$(api -X POST "$B/volumes/compose/pallet" -d "{
  \"name\": \"kernel1-ci\", \"pallet\": \"kernel1\", \"kind\": \"boot\", \"version_label\": \"ci-512\", \"lba\": 512,
  \"members\": [
    {\"name\": \"kernel\", \"role\": \"kernel\", \"kind\": \"kernel\", \"volume\": \"kernel.golden\", \"len\": \"$KERNEL_LEN\"},
    {\"name\": \"cmdline\", \"role\": \"cmdline\", \"kind\": \"bootconfig\", \"text\": \"console=ttyS0 panic=-1 boot512=ci\"}
  ]}") || die "compose/pallet"
D=$(api -X POST "$B/volumes/compose/disk" -d '{
  "name": "boot512.disk", "lba": 512,
  "partitions": [
    {"volume": "esp512.golden", "name": "EFI", "type": "esp"},
    {"volume": "kernel1-ci", "priority": 15}
  ]}') || die "compose/disk"
DISK=$(echo "$D" | j 'd["id"]')
echo "disk $DISK: disk.lba $(echo "$D" | j 'd["disk"]["lba"]'), presented at $(echo "$D" | j 'd["lba"]')"
[ "$(echo "$D" | j 'd["lba"]')" = 512 ] && echo "ok: composed at 512, presented at 512" || fail "the disk is presented at $(echo "$D" | j 'd["lba"]')"
api -X POST "$B/volumes/$DISK/seal" -d '{"force":true}' >/dev/null || fail "seal"
C=$(api -X POST "$B/volumes/$DISK/clone" -d '{"name":"boot512-clone"}') || die "clone"
CLONE=$(echo "$C" | j 'd["id"]')
[ "$(echo "$C" | j 'd["lba"]')" = 512 ] && echo "ok: the clone is 512" || fail "the clone is $(echo "$C" | j 'd["lba"]')"
PLAIN=$(api -X POST "$B/volumes" -d '{"name":"plain","size":"64M"}' | j 'd["id"]') || die "plain volume"

RA=$(api -X POST "$B/volumes/$CLONE/attach" -d "{\"transport\":\"nvme-tcp\",\"host_nqn\":\"$H1\"}") || die "attach clone"
RP=$(api -X POST "$B/volumes/$PLAIN/attach" -d "{\"transport\":\"nvme-tcp\",\"host_nqn\":\"$H1\"}") || die "attach plain"
SUB=$(echo "$RA" | j 'd["nqn"]'); NS_C=$(echo "$RA" | j 'd["nsid"]'); NS_P=$(echo "$RP" | j 'd["nsid"]')
echo "subsystem $SUB: clone nsid $NS_C, plain nsid $NS_P"

# ------------------------------------------------------------- Linux, over NVMe/TCP
say "guest initramfs: busybox, nvme-cli, nvme-tcp, vfat"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,mnt,etc/nvme,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount umount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed basename cut tr stat; do
    ln -sf busybox "$I/bin/$a"
done
cp "$NVME" "$I/bin/nvme"
ldd "$NVME" | grep -o '/[^ ]*' | while read -r lib; do mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"; done
cp "$STORMUEFI" "$I/stormuefi.efi"
: > "$I/lib/mods/order"
for m in virtio_net nvme-tcp vfat nls_cp437 nls_iso8859-1 nls_utf8; do
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
NS_C=$NS_C
NS_P=$NS_P
H1=$H1
EFI_LEN=$EFI_LEN
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
    # Native multipath lists the hidden path node (nvme0c0n1) beside the
    # head (nvme0n1): the head is the device.
    dev_of() { for b in /sys/block/nvme*n*; do case "${b##*/}" in nvme*c*n*) continue ;; esac; [ "$(cat $b/nsid 2>/dev/null)" = "$1" ] && basename $b && return; done; }
    c=$(dev_of "$NS_C"); p=$(dev_of "$NS_P")
    echo "GUEST boot clone nsid $NS_C = /dev/$c, plain nsid $NS_P = /dev/$p"
    lbs=$(cat /sys/block/$c/queue/logical_block_size); pbs=$(cat /sys/block/$c/queue/physical_block_size)
    [ "$lbs" = 512 ] && r boot-disk-logical-512 PASS || r boot-disk-logical-512 "FAIL ($lbs)"
    echo "GUEST boot disk: logical $lbs, physical $pbs"
    plbs=$(cat /sys/block/$p/queue/logical_block_size)
    [ "$plbs" = 4096 ] && r plain-volume-stays-4096 PASS || r plain-volume-stays-4096 "FAIL ($plbs)"
    parts=$(ls -d /sys/block/$c/${c}p* 2>/dev/null | wc -l)
    [ "$parts" = 2 ] && r gpt-two-partitions PASS || r gpt-two-partitions "FAIL ($parts)"
    if mount -t vfat -o ro /dev/${c}p1 /mnt 2>/tmp/m; then
        r esp-mounts PASS
        cmp -s /mnt/EFI/BOOT/BOOTX64.EFI /stormuefi.efi && r esp-holds-stormuefi PASS || r esp-holds-stormuefi FAIL
        umount /mnt
    else
        r esp-mounts "FAIL ($(cat /tmp/m))"
    fi
    nvme disconnect -n "$SUB" >/dev/null 2>&1
else
    r connect "FAIL ($(head -2 /tmp/o))"
fi
echo "GUEST dmesg:"
dmesg | grep -iE 'nvme|fat|vfat' | tail -20
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU, attach the boot clone over NVMe/TCP"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 300 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1024 -smp 2 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST)|nvme|FAT' | tail -40
results=$(tr -d '\r' < "$W/guest.log" | grep -c '^RESULT ')
bad=$(tr -d '\r' < "$W/guest.log" | grep '^RESULT ' | grep -vc ' PASS')
[ "$results" -ge 5 ] || fail "the guest reported $results results, expected 5"
[ "$bad" = 0 ] || fail "$bad guest check(s) failed"

# ------------------------------------------------------------- OVMF, the pve shape
say "OVMF boots the clone as a virtio disk, no block-size override"
api -X POST "$B/releases" -d "{\"version\":\"ci-boot512\",\"volume\":\"$CLONE\"}" >/dev/null || fail "publish"
curl -sf -m 600 -H "Authorization: Bearer $TOKEN" -o "$W/clone.img" "$B/releases/ci-boot512/image.img" \
    || die "download the clone"
echo "clone: $(stat -c %s "$W/clone.img") bytes"
if [ -r "$OVMF_VARS" ]; then
    cp "$OVMF_VARS" "$W/vars.fd"
    FW=(-drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" -drive if=pflash,format=raw,file="$W/vars.fd")
else
    FW=(-bios "$OVMF_CODE")
fi
timeout 240 qemu-system-x86_64 -machine q35,accel=$ACCEL -m 1024 -nographic -no-reboot \
    "${FW[@]}" \
    -drive file="$W/clone.img",format=raw,if=none,id=d0,snapshot=on \
    -device virtio-blk-pci,drive=d0,bootindex=1 \
    -serial file:"$W/serial.txt" -monitor none -display none >/dev/null 2>&1 || true
sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' "$W/serial.txt" | tr -d '\r' > "$W/serial.clean"
grep -a "stormuefi\|SELECTION\|kernel1\|Command line\|Linux version\|No bootable" "$W/serial.clean" | head -20
grep -qa "stormuefi" "$W/serial.clean" && echo "ok: firmware started stormuefi from the 512-byte ESP" \
    || fail "firmware did not start stormuefi"
grep -qa "Linux version" "$W/serial.clean" && echo "ok: stormuefi started the kernel from the boot pallet" \
    || fail "stormuefi did not start the kernel"
grep -qa "Command line:.*boot512=ci" "$W/serial.clean" && echo "ok: with the pallet's command line" \
    || fail "the kernel did not get the pallet's command line"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; tail -20 "$W/engine.log"; exit 1
