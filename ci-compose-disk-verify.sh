#!/usr/bin/env bash
# ci-compose-disk-verify.sh — a composed disk, checked by things that are not
# ours.
#
# The unit and HTTP tests prove the engine agrees with itself about a composed
# disk. This proves a *node* would agree: the built binary composes a disk out
# of a real kernel, a real initramfs and a real ESP, serves it as a ublk block
# device, and then
#
#   - fdisk reads the GPT at 4096-byte LBAs and finds two partitions,
#   - blkid recognises the ESP as vfat and the kernel mounts it,
#   - the kernel bytes inside the pallet digest to sha256sum of the file,
#   - `stormblock pallet verify` passes against the block device, and
#   - OVMF boots it: firmware finds the ESP on the 4Kn disk, shim loads grub,
#     grub reads the config we put on the ESP and lists both partitions.
#
# Runs on dev.g8.lo as root. Everything lives under /build/work — never /tmp,
# which is a tmpfs that a sparse image quietly fills (CLAUDE.md).
#
#   STORMBLOCK_BIN=/build/cargo/stormblock/debug/stormblock ./ci-compose-disk-verify.sh
set -euo pipefail

BIN=${STORMBLOCK_BIN:-/build/cargo/stormblock/debug/stormblock}
WORK=${WORK:-/build/work/compose-verify}
MGMT=${MGMT:-127.0.0.1:9199}
NVMEOF=${NVMEOF:-127.0.0.1:4421}
KERNEL=${KERNEL:-$(ls /boot/vmlinuz-* | head -1)}
INITRD=${INITRD:-$(ls /boot/initramfs-*.img | head -1)}
SHIM=${SHIM:-/boot/efi/EFI/BOOT/BOOTX64.EFI}
GRUB=${GRUB:-/boot/efi/EFI/BOOT/grubx64.efi}
OVMF_CODE=${OVMF_CODE:-/usr/share/edk2/ovmf/OVMF_CODE.fd}
OVMF_VARS=${OVMF_VARS:-/usr/share/edk2/ovmf/OVMF_VARS.fd}

api() { curl -sf -H 'Content-Type: application/json' "$@"; }
say() { printf '\n== %s\n' "$*"; }
fail() { printf '\nFAIL: %s\n' "$*" >&2; exit 1; }
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }

ENGINE=
UBLK=
cleanup() {
    set +e
    if [ -n "$UBLK" ]; then
        umount "$WORK/mnt" 2>/dev/null
        api -X DELETE "http://$MGMT/api/v1/volumes/$DISK_ID/attach" >/dev/null 2>&1
    fi
    [ -n "$ENGINE" ] && kill "$ENGINE" 2>/dev/null && wait "$ENGINE" 2>/dev/null
    rm -f "$WORK/slab.img" "$WORK/esp.img" "$WORK/vars.fd"
    rm -rf "$WORK/data" "$WORK/esp"
}
trap cleanup EXIT

[ -x "$BIN" ] || fail "no binary at $BIN"
[ -r "$KERNEL" ] || fail "no kernel at $KERNEL"
[ -r "$INITRD" ] || fail "no initramfs at $INITRD"
rm -rf "$WORK"; mkdir -p "$WORK/data" "$WORK/mnt" "$WORK/esp/EFI/BOOT" "$WORK/esp/EFI/fedora"

# ---------------------------------------------------------------- the engine
say "slab and engine"
truncate -s 2G "$WORK/slab.img"
"$BIN" slab format "$WORK/slab.img" >/dev/null
cat > "$WORK/stormblock.toml" <<EOF
[[drives]]
path = "$WORK/slab.img"

[management]
listen_addr = "$MGMT"
data_dir = "$WORK/data"
node_name = "compose-verify"
discovery_disabled = true
advertised_addr = "127.0.0.1"
EOF
"$BIN" -c "$WORK/stormblock.toml" --no-iscsi --nvmeof-addr "$NVMEOF" \
    --nvmeof-nqn nqn.2026-09.lo.test:compose >"$WORK/engine.log" 2>&1 &
ENGINE=$!
for _ in $(seq 1 100); do
    api "http://$MGMT/api/v1/slabs" >/dev/null 2>&1 && break
    sleep 0.1
done
api "http://$MGMT/api/v1/slabs" >/dev/null || fail "engine did not come up: $(tail -5 "$WORK/engine.log")"

# ------------------------------------------------------------------- the ESP
# A real one: shim as the default loader, grub beside it, and a grub.cfg that
# says something we can look for on the serial console. Fedora's grub looks
# for its config in /EFI/fedora, shim's stub looks in /EFI/BOOT; give both.
say "ESP with shim + grub"
if [ -r "$SHIM" ] && [ -r "$GRUB" ]; then
    cp "$SHIM" "$WORK/esp/EFI/BOOT/BOOTX64.EFI"
    cp "$GRUB" "$WORK/esp/EFI/BOOT/grubx64.efi"
    cat > "$WORK/esp/EFI/BOOT/grub.cfg" <<'EOF'
serial --unit=0 --speed=115200
terminal_output serial console
echo "COMPOSED-DISK-GRUB-UP"
ls
echo "COMPOSED-DISK-LS-DONE"
halt
EOF
    cp "$WORK/esp/EFI/BOOT/grub.cfg" "$WORK/esp/EFI/fedora/grub.cfg"
    BOOTABLE=1
else
    echo "(no shim/grub on this host — the ESP gets a marker file only, no firmware boot)"
    echo hello > "$WORK/esp/EFI/BOOT/MARKER.TXT"
    BOOTABLE=0
fi
mkfs.vfat -F 16 -n EFI -C "$WORK/esp.img" 16384 >/dev/null
mcopy -i "$WORK/esp.img" -s "$WORK/esp/EFI" ::/ >/dev/null

# --------------------------------------------------------------- the goldens
import() {
    local name=$1 file=$2
    local id
    id=$(api -X POST "http://$MGMT/api/v1/volumes/import" \
        -d "{\"name\":\"$name\",\"file\":\"$file\",\"format\":\"raw\"}" | j 'd["id"]')
    for _ in $(seq 1 600); do
        local st
        st=$(api "http://$MGMT/api/v1/volumes/import/$id")
        case "$(echo "$st" | j 'd["state"]')" in
            Done|done) echo "$st" | j 'd["volume_id"]'; return 0 ;;
            Failed|failed) fail "import $name: $(echo "$st" | j 'd.get("error")')" ;;
        esac
        sleep 0.2
    done
    fail "import $name did not finish"
}
say "goldens: kernel, initramfs, esp"
KERNEL_ID=$(import kernel.golden "$KERNEL")
INITRD_ID=$(import initrd.golden "$INITRD")
ESP_ID=$(import esp.golden "$WORK/esp.img")
KERNEL_LEN=$(stat -c %s "$KERNEL")
INITRD_LEN=$(stat -c %s "$INITRD")
echo "kernel $KERNEL_ID ($KERNEL_LEN bytes), initramfs $INITRD_ID ($INITRD_LEN bytes), esp $ESP_ID"

# ---------------------------------------------------------- the boot pallet
say "compose the boot pallet"
PALLET=$(api -X POST "http://$MGMT/api/v1/volumes/compose/pallet" -d "{
  \"name\": \"boot-v1\", \"pallet\": \"boot\", \"kind\": \"boot\", \"version_label\": \"$(basename "$KERNEL")\",
  \"members\": [
    {\"name\": \"kernel\", \"role\": \"kernel\", \"kind\": \"kernel\", \"volume\": \"kernel.golden\", \"len\": \"$KERNEL_LEN\"},
    {\"name\": \"initramfs\", \"role\": \"initramfs\", \"kind\": \"initramfs\", \"volume\": \"initrd.golden\", \"len\": \"$INITRD_LEN\"},
    {\"name\": \"cmdline\", \"role\": \"cmdline\", \"kind\": \"bootconfig\", \"text\": \"root=/dev/nvme0n1p2 ro console=ttyS0\"}
  ]}")
echo "$PALLET" | j 'f"pallet {d[\"pallet\"][\"pallet\"]} v{d[\"pallet\"][\"version\"]}: shared {d[\"pallet\"][\"shared_bytes\"]} written {d[\"pallet\"][\"written_bytes\"]} size {d[\"virtual_size_human\"]}"'
KERNEL_OFF=$(echo "$PALLET" | j 'd["pallet"]["members"][0]["offset"]')
KERNEL_DIGEST=$(echo "$PALLET" | j 'd["pallet"]["members"][0]["digest"]')
[ "$(echo "$PALLET" | j 'd["pallet"]["members"][0]["shared"]')" = True ] || fail "the kernel was not shared"
[ "$KERNEL_DIGEST" = "$(sha256sum "$KERNEL" | cut -d' ' -f1)" ] || fail "the member digest is not the file's"

# ------------------------------------------------------------------ the disk
say "compose the disk"
DISK=$(api -X POST "http://$MGMT/api/v1/volumes/compose/disk" -d '{
  "name": "node1.disk",
  "partitions": [
    {"volume": "esp.golden", "name": "EFI", "type": "esp"},
    {"volume": "boot-v1", "priority": 5}
  ]}')
DISK_ID=$(echo "$DISK" | j 'd["id"]')
echo "$DISK" | j 'f"disk {d[\"name\"]} {d[\"virtual_size_human\"]}: lba {d[\"disk\"][\"lba\"]}, gpt minted {d[\"disk\"][\"gpt_minted\"]}, written {d[\"disk\"][\"written_bytes\"]}, allocated {d[\"allocated_human\"]}"'
[ "$(echo "$DISK" | j 'd["disk"]["written_bytes"]')" = 0 ] || fail "a composed disk wrote bytes"
PALLET_START=$(echo "$DISK" | j 'd["disk"]["partitions"][1]["start_bytes"]')

# A second node: nothing minted, nothing allocated.
DISK2=$(api -X POST "http://$MGMT/api/v1/volumes/compose/disk" -d '{
  "name": "node2.disk",
  "partitions": [
    {"volume": "esp.golden", "name": "EFI", "type": "esp"},
    {"volume": "boot-v1", "priority": 5}
  ]}')
[ "$(echo "$DISK2" | j 'd["disk"]["gpt_minted"]')" = False ] || fail "the second disk minted a GPT"
[ "$(echo "$DISK2" | j 'd["allocated_bytes"]')" = 0 ] || fail "the second disk allocated"
echo "node2.disk: gpt reused, allocated $(echo "$DISK2" | j 'd["allocated_bytes"]') bytes"

# ---------------------------------------------------------- serve it: ublk
say "attach as a ublk device"
ATT=$(api -X POST "http://$MGMT/api/v1/volumes/$DISK_ID/attach" -d '{}')
UBLK=$(echo "$ATT" | j 'd.get("device_hint","")')
[ -n "$UBLK" ] || fail "attach did not yield a ublk device: $ATT"
for _ in $(seq 1 50); do [ -b "$UBLK" ] && break; sleep 0.1; done
[ -b "$UBLK" ] || fail "$UBLK never appeared"
partprobe "$UBLK" 2>/dev/null || true
udevadm settle 2>/dev/null || true
for _ in $(seq 1 50); do [ -b "${UBLK}p2" ] && break; sleep 0.1; done
[ -b "${UBLK}p2" ] || fail "the kernel did not find the partitions on $UBLK"
echo "$UBLK, partitions: $(ls ${UBLK}p*)"

# ------------------------------------------------------ external readers
say "fdisk"
fdisk -l "$UBLK" | tee "$WORK/fdisk.txt"
grep -q "Sector size (logical/physical): 4096 bytes / 4096 bytes" "$WORK/fdisk.txt" || fail "fdisk does not see 4096-byte sectors"
grep -q "Disklabel type: gpt" "$WORK/fdisk.txt" || fail "fdisk does not see a GPT"
grep -q "EFI System" "$WORK/fdisk.txt" || fail "fdisk does not see the ESP"
[ "$(grep -c "^${UBLK}p" "$WORK/fdisk.txt")" = 2 ] || fail "fdisk does not see two partitions"

say "blkid + mount the ESP"
blkid "${UBLK}p1" | tee "$WORK/blkid.txt"
grep -q 'TYPE="vfat"' "$WORK/blkid.txt" || fail "the ESP is not vfat to blkid"
mount -o ro "${UBLK}p1" "$WORK/mnt"
ls -R "$WORK/mnt" | head -20
[ -f "$WORK/mnt/EFI/BOOT/BOOTX64.EFI" ] || [ -f "$WORK/mnt/EFI/BOOT/MARKER.TXT" ] || fail "the ESP does not hold what we put in it"
umount "$WORK/mnt"

say "the kernel inside the pallet is the kernel"
GOT=$(dd if="$UBLK" bs=4096 iflag=skip_bytes,count_bytes skip=$((PALLET_START + KERNEL_OFF)) count="$KERNEL_LEN" status=none | sha256sum | cut -d' ' -f1)
[ "$GOT" = "$KERNEL_DIGEST" ] || fail "kernel bytes read off the block device do not digest to the file"
echo "sha256 $GOT == $(basename "$KERNEL")"

say "stormblock pallet, against the block device"
"$BIN" pallet --drive "$UBLK" list
"$BIN" pallet --drive "$UBLK" verify all | tee "$WORK/verify.txt"
grep -qi "fail\|error" "$WORK/verify.txt" && fail "pallet verify reported a problem"

# --------------------------------------------------------------- firmware
if [ "$BOOTABLE" = 1 ] && command -v qemu-system-x86_64 >/dev/null && [ -r "$OVMF_CODE" ]; then
    say "OVMF boots it (NVMe, 4096-byte LBAs)"
    if [ -r "$OVMF_VARS" ]; then
        cp "$OVMF_VARS" "$WORK/vars.fd"
        FW=(-drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" -drive if=pflash,format=raw,file="$WORK/vars.fd")
    else
        FW=(-bios "$OVMF_CODE")
    fi
    ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
    timeout 120 qemu-system-x86_64 -machine q35,accel=$ACCEL -m 512 -nographic -no-reboot \
        "${FW[@]}" \
        -drive file="$UBLK",format=raw,if=none,id=d0,cache=none \
        -device nvme,id=nvme0,serial=composed \
        -device nvme-ns,drive=d0,bus=nvme0,logical_block_size=4096,physical_block_size=4096 \
        -serial file:"$WORK/serial.txt" -monitor none -display none >/dev/null 2>&1 || true
    sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' "$WORK/serial.txt" | tr -d '\r' | grep -a "COMPOSED-DISK\|(hd0" | tee "$WORK/boot.txt"
    grep -q "COMPOSED-DISK-GRUB-UP" "$WORK/boot.txt" || fail "grub did not come up from the composed disk (see $WORK/serial.txt)"
    grep -q "(hd0,gpt2)" "$WORK/boot.txt" || fail "grub did not list the pallet partition"
    echo "firmware found the ESP, shim loaded grub, grub read our config and saw both partitions"
else
    echo "(firmware boot skipped)"
fi

say "PASS"
