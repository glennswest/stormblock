#!/bin/sh
# What an initramfs is for comes from its binary, and a wrong one stops the
# build (#514). The blocks of scripts/build-stormblock-initramfs.sh, against
# hand-made ELF headers. The whole image on a real kernel:
# ci-initramfs-aarch64-verify.sh.
set -u
GEN=scripts/build-stormblock-initramfs.sh
W=$(mktemp -d "${TMPDIR:-/tmp}/initramfs-arch.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
ok()   { echo "  ok    $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
same() { # what got want
    if [ "$2" = "$3" ]; then ok "$1"; else fail "$1: got '$2', want '$3'"; fi
}

sed -n '/^# --- BEGIN target architecture/,/^# --- END target architecture/p' "$GEN" > "$W/blocks"
sed -n '/^# --- BEGIN boot path/,/^# --- END boot path/p' "$GEN" >> "$W/blocks"
sed -n '/^# --- BEGIN no foreign binaries/,/^# --- END no foreign binaries/p' "$GEN" >> "$W/blocks"
[ "$(grep -c '^# --- BEGIN' "$W/blocks")" = 3 ] || { echo "FAIL: the blocks are not in $GEN"; exit 1; }
. "$W/blocks"

elf() { # file machine-low machine-high (octal)
    { printf '\177ELF'; head -c 14 /dev/zero; printf "\\$2\\$3"; head -c 44 /dev/zero; } > "$1"
}

elf "$W/x86" 076 000     # 62
elf "$W/arm" 267 000     # 183
elf "$W/riscv" 363 000   # 243
printf '#!/bin/sh\nexit 0\n' > "$W/script"

same "an x86_64 ELF"  "$(elf_machine "$W/x86")" 62
same "an aarch64 ELF" "$(elf_machine "$W/arm")" 183
same "a script is no ELF" "$(elf_machine "$W/script")" ""
same "a missing file is no ELF" "$(elf_machine "$W/none")" ""
same "62 is x86_64"   "$(machine_arch 62)" x86_64
same "183 is aarch64" "$(machine_arch 183)" aarch64
same "another machine is neither" "$(machine_arch 243)" ""

same "an aarch64 kernel, an aarch64 binary" "$(kver_refused 7.2.15-200.fc44.aarch64 aarch64)" ""
same "an x86_64 kernel, an x86_64 binary" "$(kver_refused 7.2.15-200.fc44.x86_64 x86_64)" ""
same "a kernel that names no architecture is not refused" "$(kver_refused 6.17.1-test aarch64)" ""
same "an x86_64 kernel, an aarch64 binary" \
    "$(kver_refused 7.2.15-200.fc44.x86_64 aarch64)" "kernel 7.2.15-200.fc44.x86_64 is not for aarch64"
same "an aarch64 kernel, an x86_64 binary" \
    "$(kver_refused 7.2.15-200.fc44.aarch64 x86_64)" "kernel 7.2.15-200.fc44.aarch64 is not for x86_64"

# The O6N's boot path: each module named when it is absent, built in counts.
M="$W/mods"
mkdir -p "$M/kernel/drivers/net/ethernet/realtek" "$M/kernel/drivers/nvme/host" "$M/kernel/drivers/block"
: > "$M/kernel/drivers/net/ethernet/realtek/r8169.ko.xz"
: > "$M/kernel/drivers/nvme/host/nvme.ko.xz"
: > "$M/kernel/drivers/nvme/host/nvme-tcp.ko.xz"
: > "$M/kernel/drivers/block/ublk_drv.ko.xz"
same "the whole path" "$(boot_path_missing "$M")" ""
rm "$M/kernel/drivers/net/ethernet/realtek/r8169.ko.xz"
same "no r8169: named" "$(boot_path_missing "$M")" "net/ethernet/realtek/r8169"
echo "kernel/drivers/net/ethernet/realtek/r8169.ko" > "$M/modules.builtin"
same "r8169 built in: there" "$(boot_path_missing "$M")" ""
rm "$M/kernel/drivers/nvme/host/nvme-tcp.ko.xz" "$M/kernel/drivers/block/ublk_drv.ko.xz"
same "two gone: both named" "$(boot_path_missing "$M" | tr '\n' ' ')" "nvme/host/nvme-tcp block/ublk_drv "

# A foreign program is found by its header, wherever it is; firmware and scripts
# are nobody's.
R="$W/root"
mkdir -p "$R/bin" "$R/lib64" "$R/lib/firmware/x"
elf "$R/bin/busybox" 267 000
elf "$R/lib64/libc.so.6" 267 000
elf "$R/lib/firmware/x/blob" 076 000
printf '#!/bin/sh\n' > "$R/init"
same "all aarch64, firmware aside" "$(foreign_elfs "$R" 183)" ""
elf "$R/lib64/libkmod.so.2" 076 000
same "an x86_64 library in an aarch64 image" "$(foreign_elfs "$R" 183)" "$R/lib64/libkmod.so.2"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILS"; exit 1; fi
