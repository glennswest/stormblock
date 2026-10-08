#!/usr/bin/env bash
# The boot's storage inventory on a real kernel's sysfs (#345).
#
# tests/initramfs-storage-inventory.sh runs the block against a fake tree.
# This boots dev's kernel in QEMU with an initramfs whose /init is the shipped
# `storage inventory` block, and four controllers:
#   * AHCI (q35's own) with a disk            -> sata, serial from the drive
#   * NVMe with a namespace                    -> nvme
#   * virtio-blk                               -> virtio
#   * an LSI SAS HBA (lsi53c895a, no driver in the guest) -> a WARNING
# Checked: every controller listed with its driver (or the warning), every
# drive under its controller with its serial and size, and local-disk.json
# carrying the same as JSON.
#
# Needs: qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules, a
# static busybox, python3. Unprivileged. Run on dev through sc-build:
#   sc-build 'bash ci-storage-inventory-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-storinv.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
GEN=scripts/build-stormblock-initramfs.sh

say "guest initramfs: busybox, ahci, nvme, virtio_blk, the shipped inventory block"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in $("$BUSYBOX" --list); do
    [ -e "$I/bin/$a" ] || ln -s busybox "$I/bin/$a"
done
: > "$I/lib/mods/order"
for mod in ahci nvme virtio_blk sd_mod; do
    modprobe -S "$KVER" --show-depends "$mod" 2>/dev/null | awk '$1=="insmod"{print $2}'
done | awk '!seen[$0]++' | while read -r ko; do
    base=$(basename "$ko" | sed 's/\.xz$//; s/\.zst$//')
    case "$ko" in
        *.xz) xz -dc "$ko" > "$I/lib/mods/$base" ;;
        *.zst) zstd -dcq "$ko" > "$I/lib/mods/$base" ;;
        *) cp "$ko" "$I/lib/mods/$base" ;;
    esac
    echo "$base" >> "$I/lib/mods/order"
done
echo "  modules: $(tr '\n' ' ' < "$I/lib/mods/order")"
INV=$(sed -n '/# --- BEGIN storage inventory/,/# --- END storage inventory/p' "$GEN")
[ -n "$INV" ] || { echo "FAIL: could not extract the inventory block"; exit 1; }
{
    cat <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs tmpfs /run
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
EOF
    printf '%s\n' "$INV"
    cat <<'EOF'
echo "JSON $(cat /run/stormblock/local-disk.json)"
echo "MARK done"
poweroff -f
EOF
} > "$I/init"
chmod +x "$I/init"
sh -n "$I/init" || fail "the guest /init does not parse"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
for d in sata nvme virtio; do truncate -s 1G "$W/$d.img"; done

say "boot $KERNEL in QEMU with four storage controllers"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 180 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1024 -smp 2 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -drive file="$W/sata.img",if=none,id=d0,format=raw -device ide-hd,drive=d0,bus=ide.0,serial=SATA0001,model=TestSATA \
    -drive file="$W/nvme.img",if=none,id=d1,format=raw -device nvme,drive=d1,serial=NVME0001 \
    -drive file="$W/virtio.img",if=none,id=d2,format=raw -device virtio-blk-pci,drive=d2,serial=VIRT0001 \
    -device lsi53c895a,id=scsi0 > "$W/guest.log" 2>&1
echo "  qemu exit $?"
tr -d '\r' < "$W/guest.log" > "$W/g.txt"
sed -n '/storage inventory:/,/MARK done/p' "$W/g.txt" | grep -v '^JSON' | head -30
grep -q '^MARK done' "$W/g.txt" || { tail -30 "$W/g.txt"; fail "the guest did not finish"; }

grep -q "\[8086:2922\] class 0x0106.*: ahci, 1 drive(s)" "$W/g.txt" && ok "the AHCI controller, ahci, one drive" || fail "no AHCI line"
grep -q "class 0x0108.*: nvme, 1 drive(s)" "$W/g.txt" && ok "the NVMe controller, nvme, one drive" || fail "no NVMe line"
grep -q "class 0x0100.*: virtio-pci, 1 drive(s)" "$W/g.txt" && ok "the virtio-blk controller, one drive" || fail "no virtio line"
grep -q "WARNING: storage controller .* \[1000:0012\] (class 0x0100.*) has no driver bound" "$W/g.txt" \
    && ok "the SAS HBA with no driver: a WARNING naming it" || fail "no warning for the unbound LSI controller"
grep -q "sda: TestSATA serial SATA0001, 1 GB, sata on " "$W/g.txt" && ok "sda: model, serial, size, sata" || fail "no sda line"
grep -q "nvme0n1: .* serial NVME0001, 1 GB, nvme on " "$W/g.txt" && ok "nvme0n1: serial, size, nvme" || fail "no nvme0n1 line"
grep -q "vda: .*1 GB, virtio on " "$W/g.txt" && ok "vda: size, virtio" || fail "no vda line"
python3 - "$W/g.txt" <<'PY' && ok "local-disk.json carries every controller and drive, as JSON" || fail "local-disk.json"
import json, sys
line = next(l for l in open(sys.argv[1]) if l.startswith("JSON "))
d = json.loads(line[5:])
names = sorted(x["name"] for x in d["drives"])
unbound = [c for c in d["controllers"] if c["driver"] is None]
print("  drives", names, "controllers", len(d["controllers"]), "unbound", [c["id"] for c in unbound])
assert names == ["nvme0n1", "sda", "vda"], names
assert any(c["id"] == "1000:0012" for c in unbound)
assert all(x["controller"] for x in d["drives"])
PY

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILS"; exit 1; fi
