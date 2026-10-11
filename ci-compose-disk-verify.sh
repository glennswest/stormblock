#!/usr/bin/env bash
# ci-compose-disk-verify.sh — a composed disk, checked by things that are not
# ours. Unprivileged: the kernel and the firmware are guests (#230).
#
# The unit and HTTP tests prove the engine agrees with itself about a composed
# disk. This proves a *node* would agree: the built binary composes a disk out
# of a real kernel, an initramfs and a real ESP (stormuefi), serves it over
# NVMe/TCP, and then
#
#   - the engine: the disk is presented at 4096-byte LBAs, wrote no bytes of
#     its own, and a second disk of the same layout mints no GPT and takes no
#     slot from the slab;
#   - Linux (the host's kernel in QEMU, nvme-cli over NVMe/TCP) sees 4096-byte
#     sectors and the GPT's two partitions, mounts the ESP (vfat at 4096-byte
#     sectors) and finds stormuefi in it byte for byte, and reads the kernel
#     out of the pallet partition, digesting to sha256sum of the file;
#   - `stormblock pallet verify` passes against the same namespace, read
#     through the engine's own NVMe/TCP initiator;
#   - OVMF boots the disk's bytes as a 4Kn NVMe drive: firmware finds the ESP,
#     starts stormuefi, which starts the pallet's kernel with the pallet's
#     command line and its initramfs.
#
# Before #230 this ran as root on dev with shim and grub out of /boot/efi and
# the binary at /build/cargo/…, all gone with the build VMs.
#
# Needs: cargo, git (stormuefi), qemu-system-x86_64, OVMF, mkfs.vfat + mtools,
# /boot/vmlinuz-$(uname -r) and its modules, a static busybox, nvme-cli, curl.
#   sc-build 'cargo build --locked && bash ci-compose-disk-verify.sh'
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
W=$(mktemp -d "$TMPDIR/ci-compose.XXXXXX")
PORT=$((20000 + RANDOM % 20000))
MGMT=$((PORT + 1))
TOKEN=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
H1="nqn.2014-08.org.nvmexpress:uuid:22222222-3333-4444-5555-666666666602"
SB_PID=""
FAILS=0

say()  { echo "== $*"; }
ok()   { echo "ok: $*"; }
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
for cmd in qemu-system-x86_64 mkfs.vfat mcopy curl python3 cpio; do
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
# 4096-byte sectors: the disk this lands in is presented at 4096-byte LBAs,
# and both the kernel's FAT driver and firmware's compare the BPB's
# bytes-per-sector with the media's. A 512-sector ESP on a 4Kn disk reads as
# "can't read superblock" — blkid still names it vfat, which is the trap.
# 64 MiB: FAT16 needs 4085 clusters, which at one 4 KiB sector per cluster is
# more than a 16 MiB image has.
say "ESP: FAT16, 4096-byte sectors, stormuefi as the default loader"
mkdir -p "$W/esp/EFI/BOOT"
cp "$STORMUEFI" "$W/esp/EFI/BOOT/BOOTX64.EFI"
mkfs.vfat -F 16 -S 4096 -n EFI -C "$W/esp.img" 65536 >/dev/null || die "mkfs.vfat"
mcopy -i "$W/esp.img" -s "$W/esp/EFI" ::/ || die "mcopy"

# ------------------------------------------------- the guest's initramfs
# Two jobs: booted by QEMU with `-kernel`, it attaches the composed disk over
# NVMe/TCP and checks it; booted by stormuefi out of the pallet (the command
# line says `composed=ci`), it only says it is up — that is the pallet's
# initramfs member reaching the kernel.
say "guest initramfs: busybox, nvme-cli, nvme-tcp, vfat"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,mnt,etc/nvme,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount umount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed basename cut tr stat sha256sum; do
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
cat > "$I/init" <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
if grep -q composed=ci /proc/cmdline; then
    echo "COMPOSED-DISK-INITRAMFS-UP"
    poweroff -f
fi
. /env
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up; ip link set eth0 up
ip addr add 10.0.2.15/24 dev eth0; ip route add default via 10.0.2.2
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
if nvme connect -t tcp -a 10.0.2.2 -s "$PORT" -n "$SUB" --hostnqn "$H1" >/tmp/o 2>&1; then
    sleep 3
    d=""
    for b in /sys/block/nvme*n*; do [ "$(cat $b/nsid 2>/dev/null)" = "$NSID" ] && d=$(basename $b); done
    echo "GUEST composed disk nsid $NSID = /dev/$d"
    lbs=$(cat /sys/block/$d/queue/logical_block_size)
    [ "$lbs" = 4096 ] && r sectors-4096 PASS || r sectors-4096 "FAIL ($lbs)"
    parts=$(ls -d /sys/block/$d/${d}p* 2>/dev/null | wc -l)
    [ "$parts" = 2 ] && r gpt-two-partitions PASS || r gpt-two-partitions "FAIL ($parts)"
    if mount -t vfat -o ro /dev/${d}p1 /mnt 2>/tmp/m; then
        r esp-mounts PASS
        cmp -s /mnt/EFI/BOOT/BOOTX64.EFI /stormuefi.efi && r esp-holds-stormuefi PASS || r esp-holds-stormuefi FAIL
        umount /mnt
    else
        r esp-mounts "FAIL ($(cat /tmp/m))"
    fi
    # The kernel member, read off the block device: whole 4 KiB blocks from
    # the pallet's start plus the member's offset, cut to its length.
    got=$(dd if=/dev/$d bs=4096 skip=$((KSTART / 4096)) count=$(((KLEN + 4095) / 4096)) 2>/dev/null \
        | head -c "$KLEN" | sha256sum | cut -d' ' -f1)
    [ "$got" = "$KDIGEST" ] && r kernel-in-pallet-is-the-kernel PASS || r kernel-in-pallet-is-the-kernel "FAIL ($got)"
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
# The pallet's copy carries no /env: as a pallet member it only says it is up.
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/pallet-initrd.gz"

# ---------------------------------------------------------------- the engine
say "engine on 127.0.0.1:$PORT (NVMe/TCP) and :$MGMT (API)"
mkdir -p "$W/data"
truncate -s 2G "$W/d1.img"
cat > "$W/stormblock.toml" <<EOF
[management]
api_token = "$TOKEN"
admin_token = "$TOKEN"
listen_addr = "127.0.0.1:$MGMT"
data_dir = "$W/data"
node_name = "ci-compose"
advertised_addr = "10.0.2.2"
discovery_disabled = true

[nvmeof]
export_drives = false
EOF
RUST_LOG=stormblock=info "$BIN" --config "$W/stormblock.toml" \
    --device "$W/d1.img" --data-dir "$W/data" --no-iscsi \
    --nvmeof-addr "127.0.0.1:$PORT" --nvmeof-nqn "nqn.2026-09.lo.storm:ci-compose" \
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
say "goldens: kernel, initramfs, ESP"
import kernel.golden "$KERNEL" >/dev/null
import initrd.golden "$W/pallet-initrd.gz" >/dev/null
import esp.golden "$W/esp.img" >/dev/null
KERNEL_LEN=$(stat -c %s "$KERNEL")
INITRD_LEN=$(stat -c %s "$W/pallet-initrd.gz")

# ------------------------------------------------------------ the boot pallet
say "compose the boot pallet"
P=$(api -X POST "$B/volumes/compose/pallet" -d "{
  \"name\": \"boot-v1\", \"pallet\": \"kernel1\", \"kind\": \"boot\", \"version_label\": \"ci-compose\",
  \"members\": [
    {\"name\": \"kernel\", \"role\": \"kernel\", \"kind\": \"kernel\", \"volume\": \"kernel.golden\", \"len\": \"$KERNEL_LEN\"},
    {\"name\": \"initramfs\", \"role\": \"initramfs\", \"kind\": \"initramfs\", \"volume\": \"initrd.golden\", \"len\": \"$INITRD_LEN\"},
    {\"name\": \"cmdline\", \"role\": \"cmdline\", \"kind\": \"bootconfig\", \"text\": \"console=ttyS0 panic=-1 composed=ci\"}
  ]}") || die "compose/pallet"
echo "pallet v$(echo "$P" | j 'd["pallet"]["version"]'): shared $(echo "$P" | j 'd["pallet"]["shared_bytes"]') written $(echo "$P" | j 'd["pallet"]["written_bytes"]')"
KERNEL_OFF=$(echo "$P" | j 'd["pallet"]["members"][0]["offset"]')
KERNEL_DIGEST=$(echo "$P" | j 'd["pallet"]["members"][0]["digest"]')
[ "$(echo "$P" | j 'd["pallet"]["members"][0]["shared"]')" = True ] && ok "the kernel is shared in, not copied" \
    || fail "the kernel was not shared"
[ "$KERNEL_DIGEST" = "$(sha256sum "$KERNEL" | cut -d' ' -f1)" ] && ok "the member digest is the file's" \
    || fail "the member digest is not the file's"

# ------------------------------------------------------------------ the disk
say "compose the disk"
DISK=$(api -X POST "$B/volumes/compose/disk" -d '{
  "name": "node1.disk",
  "partitions": [
    {"volume": "esp.golden", "name": "EFI", "type": "esp"},
    {"volume": "boot-v1", "priority": 15}
  ]}') || die "compose/disk"
DISK_ID=$(echo "$DISK" | j 'd["id"]')
echo "disk: lba $(echo "$DISK" | j 'd["disk"]["lba"]'), presented at $(echo "$DISK" | j 'd["lba"]'), gpt minted $(echo "$DISK" | j 'd["disk"]["gpt_minted"]'), written $(echo "$DISK" | j 'd["disk"]["written_bytes"]')"
[ "$(echo "$DISK" | j 'd["lba"]')" = 4096 ] && ok "presented at 4096" || fail "presented at $(echo "$DISK" | j 'd["lba"]')"
[ "$(echo "$DISK" | j 'd["disk"]["written_bytes"]')" = 0 ] && ok "the composed disk wrote nothing" \
    || fail "a composed disk wrote bytes"
PALLET_START=$(echo "$DISK" | j 'd["disk"]["partitions"][1]["start_bytes"]')

# A second node: nothing minted, and the slab has not lost a slot. (A composed
# disk's `allocated_bytes` counts what it maps, shared or not, so the slab's
# free count is the number that says whether anything was written.)
free_slots() { api "$B/slabs" | j 'sum(s["free_slots"] for s in (d["items"] if isinstance(d, dict) else d))'; }
FREE_BEFORE=$(free_slots)
DISK2=$(api -X POST "$B/volumes/compose/disk" -d '{
  "name": "node2.disk",
  "partitions": [
    {"volume": "esp.golden", "name": "EFI", "type": "esp"},
    {"volume": "boot-v1", "priority": 15}
  ]}') || die "compose/disk node2"
[ "$(echo "$DISK2" | j 'd["disk"]["gpt_minted"]')" = False ] && ok "the second disk reused the GPT" \
    || fail "the second disk minted a GPT"
[ "$(free_slots)" = "$FREE_BEFORE" ] && ok "the second disk took no slot (free $FREE_BEFORE)" \
    || fail "the second disk took slots from the slab"

# ---------------------------------------------------- Linux, over NVMe/TCP
RA=$(api -X POST "$B/volumes/$DISK_ID/attach" -d "{\"transport\":\"nvme-tcp\",\"host_nqn\":\"$H1\"}") || die "attach"
SUB=$(echo "$RA" | j 'd["nqn"]'); NSID=$(echo "$RA" | j 'd["nsid"]')
echo "subsystem $SUB nsid $NSID"
cat > "$I/env" <<EOF
PORT=$PORT
SUB=$SUB
NSID=$NSID
H1=$H1
KSTART=$((PALLET_START + KERNEL_OFF))
KLEN=$KERNEL_LEN
KDIGEST=$KERNEL_DIGEST
EOF
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU, attach the composed disk over NVMe/TCP"
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

# ------------------------------------- stormblock pallet, against the namespace
say "stormblock pallet verify, through the engine's NVMe/TCP initiator"
URI="nvme-tcp://127.0.0.1:$PORT/$SUB?nsid=$NSID"
STORMBLOCK_HOST_NQN="$H1" "$BIN" pallet --drive "$URI" list 2>&1 | tail -5
if STORMBLOCK_HOST_NQN="$H1" "$BIN" pallet --drive "$URI" verify all > "$W/verify.txt" 2>&1; then
    cat "$W/verify.txt"
    grep -qi "fail\|error" "$W/verify.txt" && fail "pallet verify reported a problem" || ok "pallet verify passed"
else
    cat "$W/verify.txt"; fail "pallet verify exited non-zero"
fi

# --------------------------------------------------------------- firmware
say "OVMF boots it (NVMe, 4096-byte LBAs)"
api -X POST "$B/volumes/$DISK_ID/seal" -d '{"force":true}' >/dev/null || fail "seal"
api -X POST "$B/releases" -d "{\"version\":\"ci-compose\",\"volume\":\"$DISK_ID\"}" >/dev/null || fail "publish"
curl -sf -m 600 -H "Authorization: Bearer $TOKEN" -o "$W/disk.img" "$B/releases/ci-compose/image.img" \
    || die "download the disk"
echo "disk image: $(stat -c %s "$W/disk.img") bytes"
if [ -r "$OVMF_VARS" ]; then
    cp "$OVMF_VARS" "$W/vars.fd"
    FW=(-drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" -drive if=pflash,format=raw,file="$W/vars.fd")
else
    FW=(-bios "$OVMF_CODE")
fi
timeout 240 qemu-system-x86_64 -machine q35,accel=$ACCEL -m 1024 -nographic -no-reboot \
    "${FW[@]}" \
    -drive file="$W/disk.img",format=raw,if=none,id=d0,snapshot=on \
    -device nvme,id=nvme0,serial=composed \
    -device nvme-ns,drive=d0,bus=nvme0,logical_block_size=4096,physical_block_size=4096,bootindex=1 \
    -serial file:"$W/serial.txt" -monitor none -display none >/dev/null 2>&1 || true
sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' "$W/serial.txt" | tr -d '\r' > "$W/serial.clean"
grep -a "stormuefi\|SELECTION\|kernel1\|Command line\|Linux version\|No bootable\|COMPOSED-DISK" "$W/serial.clean" | head -20
grep -qa "stormuefi" "$W/serial.clean" && ok "firmware started stormuefi from the 4096-sector ESP" \
    || fail "firmware did not start stormuefi"
grep -qa "Linux version" "$W/serial.clean" && ok "stormuefi started the kernel from the boot pallet" \
    || fail "stormuefi did not start the kernel"
grep -qa "Command line:.*composed=ci" "$W/serial.clean" && ok "with the pallet's command line" \
    || fail "the kernel did not get the pallet's command line"
grep -qa "COMPOSED-DISK-INITRAMFS-UP" "$W/serial.clean" && ok "and the pallet's initramfs ran" \
    || fail "the pallet's initramfs did not run"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; tail -20 "$W/engine.log"; exit 1
