#!/usr/bin/env bash
# ci-local-boot-verify.sh — an installed disk boots on its own (#123), checked
# by things that are not ours.
#
# The unit tests prove our reader agrees with our writer. That proves nothing
# about firmware — the ext4 and ESP work both found that out the hard way — so
# this builds two releases the way a node receives them and then asks real
# tools, and real firmware, about the disk the engine lays:
#
#   - two images, A and B, each an ESP (stormuefi, 4096-byte sectors: what
#     NVMe/TCP presents) and a `kind = boot` pallet holding a real kernel;
#   - a node disk laid by `image lay-node` at 512-byte LBAs, as a real drive
#     is read, and made bootable from A, then from B, by `image local-boot`;
#   - sfdisk reads the table at 512 and finds an EFI System partition,
#     fsck.fat passes the ESP, mtools finds stormuefi in it byte for byte;
#   - `stormblock pallet verify` passes on the disk; a second run changes
#     nothing; B ranks above A;
#   - OVMF, with **only the node disk attached**, finds the ESP, starts
#     stormuefi, which selects B from the disk's own ladder and starts the
#     kernel with B's command line.
#
# Runs as the build user under sc-build (no root, no loop devices):
#
#   sc-build 'cargo build --locked && ./ci-local-boot-verify.sh'
set -euo pipefail

BIN=${STORMBLOCK_BIN:-${CARGO_TARGET_DIR:-target}/debug/stormblock}
WORK=${WORK:-$PWD/tmp/local-boot-verify}
KERNEL=${KERNEL:-$(ls /boot/vmlinuz-* 2>/dev/null | grep -v rescue | tail -1)}
OVMF_CODE=${OVMF_CODE:-/usr/share/edk2/ovmf/OVMF_CODE.fd}
OVMF_VARS=${OVMF_VARS:-/usr/share/edk2/ovmf/OVMF_VARS.fd}
STORMUEFI=${STORMUEFI:-}

say() { printf '\n== %s\n' "$*"; }
fail() { printf '\nFAIL: %s\n' "$*" >&2; exit 1; }

[ -x "$BIN" ] || fail "no binary at $BIN"
[ -r "$KERNEL" ] || fail "no readable kernel at $KERNEL"
rm -rf "$WORK"; mkdir -p "$WORK"

# ------------------------------------------------------------------ stormuefi
if [ -z "$STORMUEFI" ]; then
    say "stormuefi, built from its repo"
    # Outside this checkout: inside it, cargo takes stormuefi for a stray
    # member of stormblock's workspace and refuses to build it.
    UEFI_SRC=$(mktemp -d "$(dirname "$PWD")/stormuefi.XXXXXX")
    trap 'rm -rf "$UEFI_SRC"' EXIT
    git clone -q --depth 1 https://github.com/glennswest/stormuefi "$UEFI_SRC/src"
    (cd "$UEFI_SRC/src" && CARGO_TARGET_DIR="$UEFI_SRC/target" \
        cargo build -q --release --target x86_64-unknown-uefi)
    cp "$UEFI_SRC/target/x86_64-unknown-uefi/release/stormuefi.efi" "$WORK/stormuefi.efi"
    STORMUEFI="$WORK/stormuefi.efi"
fi
[ -r "$STORMUEFI" ] || fail "no stormuefi at $STORMUEFI"
echo "stormuefi: $(stat -c %s "$STORMUEFI") bytes; kernel: $KERNEL"

# ------------------------------------------------------------------ releases
release() {
    local tag=$1 dir="$WORK/$1"
    mkdir -p "$dir/esp/EFI/BOOT"
    cp "$STORMUEFI" "$dir/esp/EFI/BOOT/BOOTX64.EFI"
    echo "release $tag" > "$dir/esp/EFI/BOOT/RELEASE.TXT"
    cat > "$dir/image.toml" <<EOF
name = "local-boot-$tag"
block_size = 4096

[esp]
size = "64M"
label = "EFI"
from_dir = "esp"

[[pallet]]
name = "kernel1"
kind = "boot"
version_label = "ci-$tag"
priority = 15
members = [
  { name = "kernel",  role = "kernel",  kind = "kernel",     file = "$KERNEL" },
  { name = "cmdline", role = "cmdline", kind = "bootconfig", text = "console=ttyS0 panic=-1 localboot=ci-$tag" },
]
EOF
    (cd "$dir" && "$BIN" image build --spec image.toml --out image.img >build.log 2>&1) \
        || fail "image build $tag: $(tail -5 "$dir/build.log")"
}
say "two releases, A and B"
release A
release B

# ------------------------------------------------------------------ the disk
say "a node disk at 512-byte LBAs"
truncate -s 4G "$WORK/node.disk"
"$BIN" image lay-node --disk "$WORK/node.disk" --lba 512 --boot-area 1G --system 1G

local_boot() {
    "$BIN" image local-boot --disk "$WORK/node.disk" --from "$WORK/$1/image.img" \
        2>/dev/null | tee "$WORK/local-boot-$1-$2.txt"
}
say "local boot from A"
local_boot A 1
grep -q "ESP rebuilt at 512-byte sectors from 4096" "$WORK/local-boot-A-1.txt" || fail "the ESP was not rebuilt at 512"
grep -q "boot pallet kernel1 .* copied and verified" "$WORK/local-boot-A-1.txt" || fail "no boot pallet copied"
grep -q "boots on its own" "$WORK/local-boot-A-1.txt" || fail "not bootable"

say "sfdisk reads the table at 512"
sfdisk -J "$WORK/node.disk" > "$WORK/table.json"
python3 - "$WORK/table.json" <<'PY' || fail "sfdisk does not see what firmware needs"
import json, sys
t = json.load(open(sys.argv[1]))["partitiontable"]
assert t["label"] == "gpt", t["label"]
assert t.get("sectorsize", 512) == 512, t.get("sectorsize")
esp = [p for p in t["partitions"] if p["type"].upper() == "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"]
assert len(esp) == 1, "no single EFI System partition"
names = sorted(p.get("name", "") for p in t["partitions"])
print("partitions:", names)
for want in ("EFI", "kernel1", "stormblock", "stormblock-data"):
    assert want in names, want
data = [p for p in t["partitions"] if p.get("name") == "stormblock-data"][0]
assert all(p["start"] <= data["start"] for p in t["partitions"]), "the data half is not last"
open(sys.argv[1] + ".esp", "w").write(f'{esp[0]["start"]} {esp[0]["size"]}\n')
PY
read -r ESP_START ESP_SIZE < "$WORK/table.json.esp"
dd if="$WORK/node.disk" of="$WORK/esp.img" bs=512 skip="$ESP_START" count="$ESP_SIZE" status=none

say "fsck.fat and mtools on the ESP"
fsck.fat -n "$WORK/esp.img" || fail "fsck.fat does not pass the ESP"
python3 -c "import sys; b=open(sys.argv[1],'rb').read(512); print('bytes per sector:', int.from_bytes(b[11:13],'little'))" "$WORK/esp.img"
mdir -i "$WORK/esp.img" ::/EFI/BOOT
mcopy -n -i "$WORK/esp.img" ::/EFI/BOOT/BOOTX64.EFI "$WORK/bootx64.efi"
cmp "$WORK/bootx64.efi" "$STORMUEFI" || fail "stormuefi on the disk differs from the one built"

say "pallet verify on the disk"
"$BIN" pallet --drive "$WORK/node.disk" verify all | tee "$WORK/verify.txt"
grep -qi "fail\|error" "$WORK/verify.txt" && fail "pallet verify reported a problem"

say "a second run changes nothing"
local_boot A 2
grep -q "ESP already current" "$WORK/local-boot-A-2.txt" || fail "the ESP was rewritten"
grep -q "1 boot pallet(s) already present" "$WORK/local-boot-A-2.txt" || fail "the pallet was copied again"

say "the next release, B"
local_boot B 1
grep -q "ladder kernel1 v1 priority 14" "$WORK/local-boot-B-1.txt" || fail "B is not on top"
grep -q "ladder kernel1 v1 priority 13" "$WORK/local-boot-B-1.txt" || fail "A is not kept as the fallback"
"$BIN" pallet --drive "$WORK/node.disk" chain --kind boot | tee "$WORK/chain.txt"
python3 - "$WORK/chain.txt" <<'PY' || fail "the chain is not B (14) then A (13)"
import sys
lines = [l for l in open(sys.argv[1]) if "kernel1" in l]
assert len(lines) == 2, lines
assert "pri=14" in lines[0] and "pri=13" in lines[1], lines
PY

# ------------------------------------------------------------------ firmware
say "OVMF boots the disk alone"
[ -r "$OVMF_CODE" ] || fail "no OVMF at $OVMF_CODE"
cp "$OVMF_VARS" "$WORK/vars.fd"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 180 qemu-system-x86_64 -machine q35,accel=$ACCEL -m 1024 -nographic -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file="$WORK/vars.fd" \
    -drive file="$WORK/node.disk",format=raw,if=virtio \
    -serial file:"$WORK/serial.txt" -monitor none -display none >/dev/null 2>&1 || true
sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' "$WORK/serial.txt" | tr -d '\r' > "$WORK/serial.clean"
grep -a "stormuefi\|SELECTION\|kernel1\|Command line\|Linux version" "$WORK/serial.clean" | head -30
grep -qa "stormuefi" "$WORK/serial.clean" || fail "firmware did not start stormuefi from the disk (see $WORK/serial.txt)"
grep -qa "Linux version" "$WORK/serial.clean" || fail "stormuefi did not start the kernel"
grep -qa "Command line:.*localboot=ci-B" "$WORK/serial.clean" || fail "the kernel did not get B's command line"
echo "firmware found the 512-byte ESP, stormuefi selected B from the disk's ladder, the kernel started"

say "PASS"
