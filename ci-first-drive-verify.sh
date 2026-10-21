#!/usr/bin/env bash
# The system drive's choice on a real kernel's sysfs (#478).
#
# tests/initramfs-boot-hook.sh runs the survey against a sysfs made of
# directories. This boots the build VM's kernel in QEMU with drives on four
# kinds of controller, each with its own serial and WWN, and runs the shipped
# blocks (shelves, the survey's drive helpers, the drive choice) against
# the guest's real /sys/block:
#   * the machine's first drive is by controller and port order: on q35 the
#     NVMe (00:04.0) comes before virtio-scsi (00:05.0) and AHCI (00:1f.2), whatever
#     the kernel named them; a USB stick and a removable drive are never it;
#   * with the NVMe gone, AHCI port 1 before port 4;
#   * system_drive by serial, by WWN and by model + serial finds exactly the
#     drive QEMU was told, on SATA, SCSI and NVMe; a model two drives share
#     matches both; a serial that is nowhere matches none.
#
# Needs: qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules, a
# static busybox. Unprivileged:  sc-build 'bash ci-first-drive-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-firstdrive.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }
GEN=scripts/build-stormblock-initramfs.sh

say "guest initramfs: busybox, the storage drivers, the shipped blocks"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in $("$BUSYBOX" --list); do [ -e "$I/bin/$a" ] || ln -s busybox "$I/bin/$a"; done
: > "$I/lib/mods/order"
for m in ahci nvme virtio_scsi sd_mod usb_storage xhci_pci uas; do
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
SURVEY=$(sed -n '/# --- BEGIN assimilate survey/,/# --- END assimilate survey/p' "$GEN")
{
    echo '#!/bin/sh'
    echo 'export PATH=/bin'
    echo 'mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev'
    echo 'for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done'
    echo 'sleep 3'
    sed -n '/# --- BEGIN shelves/,/# --- END shelves/p' "$GEN"
    # The survey's helpers and the drive choice, as functions only: the
    # survey's own flow is the boot-hook test's.
    printf '%s\n' "$SURVEY" | sed -n '/^    drive_external() {/,/^    }/p'
    printf '%s\n' "$SURVEY" | sed -n '/^    drive_serial() {/,/^    }/p; /^    drive_model() {/,/^    }/p; /^    drive_wwn() {/,/^    }/p; /^    drive_desc() {/,/^    }/p'
    printf '%s\n' "$SURVEY" | sed -n '/# --- BEGIN drive choice/,/^    SD_FROM=""/p' | sed '$d'
    cat <<'EOF'
SURVEY_SYS=/sys/block; ALLOW_EXTERNAL=""
for d in /sys/block/sd* /sys/block/nvme*n*; do
    [ -e "$d" ] || continue
    echo "DRIVE $(drive_desc "$d" "/dev/${d##*/}") wwn $(drive_wwn "$d") path $(readlink -f "$d")"
done
fd=$(first_drive); echo "FIRST ${fd%%	*}"
for spec in "serial=SATA-PORT4" "serial=NVME-0001" "wwn=0x5000c500a0000005" "serial=SCSI-0001" "model=QEMU HARDDISK" "serial=NOWHERE"; do
    m=""
    for d in $(sd_drives); do sd_matches "$d" "$spec" && m="$m /dev/${d##*/}"; done
    echo "MATCH $spec ->$m"
done
echo "DONE"
poweroff -f
EOF
} > "$I/init"
chmod +x "$I/init"
sh -n "$I/init" || fail "the guest /init does not parse"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
for d in a b c d e f; do truncate -s 64M "$W/$d.img"; done

boot() { # extra qemu args...
    ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
    timeout 240 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 512 -smp 2 \
        -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
        -append "console=ttyS0 panic=-1 loglevel=3" "$@" > "$W/guest.log" 2>&1
    tr -d '\r' < "$W/guest.log" | grep -E '^(DRIVE|FIRST|MATCH|DONE)' | tee "$W/g.txt"
    grep -q '^DONE' "$W/g.txt" || { tail -20 "$W/guest.log"; fail "the guest did not finish"; }
}
dev_of() { grep "^DRIVE .*serial $1," "$W/g.txt" | sed 's/^DRIVE \([^:]*\):.*/\1/'; }
usb_dev() { grep "^DRIVE .*path [^ ]*/usb" "$W/g.txt" | sed 's/^DRIVE \([^:]*\):.*/\1/'; }
want() { # name expected actual
    if [ "$2" = "$3" ]; then echo "  ok    $1"; else fail "$1: expected '$2', got '$3'"; fi
}

say "NVMe, virtio-scsi, AHCI ports 1 and 4, a USB stick"
boot \
    -device nvme,id=nv,serial=NVME-0001,bus=pcie.0,addr=0x4 -drive if=none,id=dn,file="$W/a.img",format=raw -device nvme-ns,drive=dn,bus=nv \
    -device virtio-scsi-pci,id=vs,bus=pcie.0,addr=0x5 -drive if=none,id=ds,file="$W/b.img",format=raw -device scsi-hd,drive=ds,bus=vs.0,serial=SCSI-0001,wwn=0x5000c500a0000005 \
    -drive if=none,id=d4,file="$W/c.img",format=raw -device ide-hd,drive=d4,bus=ide.4,serial=SATA-PORT4,wwn=0x5000c500a0000004 \
    -drive if=none,id=d1,file="$W/d.img",format=raw -device ide-hd,drive=d1,bus=ide.1,serial=SATA-PORT1,wwn=0x5000c500a0000001 \
    -device qemu-xhci,id=xh,bus=pcie.0,addr=0x3 -drive if=none,id=du,file="$W/e.img",format=raw -device usb-storage,drive=du,bus=xh.0,serial=USB-STICK
N=$(dev_of NVME-0001); S5=$(dev_of SCSI-0001); P4=$(dev_of SATA-PORT4); P1=$(dev_of SATA-PORT1); U=$(usb_dev)
[ -n "$N" ] && [ -n "$S5" ] && [ -n "$P4" ] && [ -n "$P1" ] && [ -n "$U" ] || fail "the guest did not see every drive (nvme '$N', scsi '$S5', port4 '$P4', port1 '$P1', usb '$U')"
want "the first drive: the NVMe on 00:04.0, the lowest controller" "$N" "$(sed -n 's/^FIRST //p' "$W/g.txt")"
want "system_drive serial on SATA (VPD page 0x80)" " $P4" "$(sed -n 's/^MATCH serial=SATA-PORT4 ->//p' "$W/g.txt")"
want "system_drive serial on NVMe" " $N" "$(sed -n 's/^MATCH serial=NVME-0001 ->//p' "$W/g.txt")"
want "system_drive wwn on SCSI, 0x-spelt" " $S5" "$(sed -n 's/^MATCH wwn=0x5000c500a0000005 ->//p' "$W/g.txt")"
want "system_drive serial on SCSI" " $S5" "$(sed -n 's/^MATCH serial=SCSI-0001 ->//p' "$W/g.txt")"
want "a serial that is nowhere: none" "" "$(sed -n 's/^MATCH serial=NOWHERE ->//p' "$W/g.txt")"
grep -q "^FIRST $U$" "$W/g.txt" && fail "the USB stick was the first drive" || echo "  ok    the USB stick ($U) is not the first drive"
m=$(sed -n 's/^MATCH model=QEMU HARDDISK ->//p' "$W/g.txt" | wc -w)
[ "$m" -ge 2 ] && echo "  ok    a model several drives share matches them all ($m)" || fail "model QEMU HARDDISK matched $m drive(s)"

say "AHCI alone, port 4 enumerated first, and the USB stick"
boot \
    -drive if=none,id=d4,file="$W/c.img",format=raw -device ide-hd,drive=d4,bus=ide.4,serial=SATA-PORT4 \
    -drive if=none,id=d1,file="$W/d.img",format=raw -device ide-hd,drive=d1,bus=ide.1,serial=SATA-PORT1 \
    -device qemu-xhci,id=xh,bus=pcie.0,addr=0x3 -drive if=none,id=du,file="$W/e.img",format=raw -device usb-storage,drive=du,bus=xh.0,serial=USB-STICK
P1=$(dev_of SATA-PORT1)
want "the first drive: AHCI port 1, not port 4, never the USB stick" "$P1" "$(sed -n 's/^FIRST //p' "$W/g.txt")"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
