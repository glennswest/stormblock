#!/usr/bin/env bash
# Verify a built image against tools that are not ours.
#
# Everything in tests/integration_image.rs proves this code agrees with itself,
# which — as the ext4 work already taught us once — proves nothing about a
# consumer. This script hands the output to mtools, xorriso and fdisk, and
# rebuilds every container format with an independent parser, comparing the
# result to the raw image byte for byte.
#
# Needs: mtools, xorriso, python3. fdisk and qemu-img are used when present.
set -euo pipefail

BIN="${BIN:-./target/debug/stormblock}"
WORK="${WORK:-$(mktemp -d)}"
export MTOOLS_SKIP_CHECK=1

say() { printf '\n=== %s ===\n' "$*"; }
need() { command -v "$1" >/dev/null || { echo "SKIP: $1 not installed"; return 1; }; }

say "building sources in $WORK"
mkdir -p "$WORK/esp/EFI/BOOT" "$WORK/esp/loader/entries"
head -c 900000 /dev/urandom > "$WORK/esp/EFI/BOOT/BOOTX64.EFI"
printf 'title StormCOS\nlinux /vmlinuz\noptions root=ublk0 ro\n' \
  > "$WORK/esp/loader/entries/stormcos-6.12.0.conf"
head -c 4000000 /dev/urandom > "$WORK/vmlinuz"
head -c 2500000 /dev/urandom > "$WORK/initramfs.img"

# 24M keeps the ESP FAT16 and inside El Torito's 16-bit sector count, which is
# what an ISO needs; a disk-only image would normally take a larger FAT32 one.
cat > "$WORK/image.toml" <<EOF
name = "stormcos"
size = "320M"

[esp]
size = "24M"
label = "EFI"
from_dir = "esp"

[[pallet]]
name = "stormcos-boot"
kind = "boot"
version_label = "6.12.0-200.fc41"
priority = 15
members = [
  { name = "kernel", role = "kernel", kind = "kernel", file = "vmlinuz" },
  { name = "initramfs", role = "initramfs", kind = "initramfs", file = "initramfs.img" },
  { name = "cmdline", role = "cmdline", kind = "bootconfig", text = "root=ublk0 ro" },
]

[[pallet]]
name = "platform"
kind = "system"
members = [ { name = "rootimg", role = "rootimage", kind = "rootimage", file = "initramfs.img" } ]

[slab]
size = "rest"
tier = "hot"
EOF

ABS_BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
cd "$WORK"

say "build"
"$ABS_BIN" image build --spec image.toml --out disk.img
"$ABS_BIN" image inspect disk.img

say "GPT, per fdisk"
if need fdisk; then fdisk disk.img | head -8; fi

say "ESP, per mtools"
mdir -i disk.img@@1M ::/
mdir -i disk.img@@1M ::/EFI/BOOT
mcopy -i disk.img@@1M ::/EFI/BOOT/BOOTX64.EFI out.efi
mcopy -i disk.img@@1M ::/loader/entries/stormcos-6.12.0.conf out.conf
cmp esp/EFI/BOOT/BOOTX64.EFI out.efi
cmp esp/loader/entries/stormcos-6.12.0.conf out.conf
echo "ESP contents match, long names included"

say "container formats, rebuilt by an independent parser"
"$ABS_BIN" image convert --in disk.img --out disk.qcow2
"$ABS_BIN" image convert --in disk.img --out disk.vhd
"$ABS_BIN" image convert --in disk.img --out disk.vmdk
python3 - <<'PY'
import struct, hashlib
raw = open('disk.img','rb').read()
def same(name, data):
    ok = data == raw
    print(f"  {name:<7} {'MATCH' if ok else 'MISMATCH'}  {hashlib.sha256(data).hexdigest()[:16]}")
    assert ok, name

d = open('disk.qcow2','rb').read()
assert d[:4] == b'QFI\xfb'
cbits = struct.unpack('>I', d[20:24])[0]; size = struct.unpack('>Q', d[24:32])[0]
l1n = struct.unpack('>I', d[36:40])[0]; l1o = struct.unpack('>Q', d[40:48])[0]
rco = struct.unpack('>Q', d[48:56])[0]
assert struct.unpack('>I', d[4:8])[0] == 3 and size == len(raw)
cl = 1 << cbits; l2e = cl // 8
out = bytearray(size)
for i in range(l1n):
    l1 = struct.unpack('>Q', d[l1o+i*8:l1o+i*8+8])[0]
    if not l1: continue
    assert l1 >> 63, "COPIED flag"
    l2o = l1 & 0x00FFFFFFFFFFFE00
    for j in range(l2e):
        e = struct.unpack('>Q', d[l2o+j*8:l2o+j*8+8])[0]
        if not e: continue
        o = e & 0x00FFFFFFFFFFFE00; k = i*l2e + j
        out[k*cl:(k+1)*cl] = d[o:o+cl]
rb = struct.unpack('>Q', d[rco:rco+8])[0]
blk = d[rb:rb+cl]
bad = [c for c in range(len(d)//cl) if struct.unpack('>H', blk[c*2:c*2+2])[0] != 1]
assert not bad, f"refcount != 1 at {bad[:5]}"
same("qcow2", bytes(out))

d = open('disk.vhd','rb').read()
f = d[-512:]
assert f[:8] == b'conectix' and struct.unpack('>I', f[60:64])[0] == 2
chk = bytearray(f); stored = struct.unpack('>I', f[64:68])[0]; chk[64:68] = b'\0\0\0\0'
assert (~sum(chk)) & 0xFFFFFFFF == stored, "footer checksum"
same("vhd", d[:-512])

d = open('disk.vmdk','rb').read()
assert d[:4] == b'KDMV'
cap, grain, doff, dsize = struct.unpack('<QQQQ', d[12:44])
gtes = struct.unpack('<I', d[44:48])[0]; gd = struct.unpack('<Q', d[56:64])[0]
desc = d[doff*512:(doff+dsize)*512].split(b'\0')[0].decode()
assert 'monolithicSparse' in desc and f'RW {cap} SPARSE' in desc
gb = grain*512
out = bytearray(cap*512)
for g in range((len(raw)+gb-1)//gb):
    gt = struct.unpack('<I', d[gd*512+(g//gtes)*4: gd*512+(g//gtes)*4+4])[0]
    e = struct.unpack('<I', d[gt*512+(g%gtes)*4: gt*512+(g%gtes)*4+4])[0]
    if e: out[g*gb:(g+1)*gb] = d[e*512:e*512+gb]
same("vmdk", bytes(out[:len(raw)]))
PY

say "ISO, per xorriso"
"$ABS_BIN" image convert --in disk.img --out disk.iso --format iso
if need xorriso; then
  xorriso -indev disk.iso -toc -report_el_torito plain -report_system_area plain 2>&1 \
    | grep -E "Volume id|Boot record|El Torito|System area summary|WARNING|FAILURE"
fi
"$ABS_BIN" image inspect disk.iso

say "the pallets inside the ISO still verify"
"$ABS_BIN" pallet --drive disk.iso verify all

say "PASS — $WORK"
