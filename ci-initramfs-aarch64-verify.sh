#!/usr/bin/env bash
# The aarch64 initramfs for the Orion O6N, built on x86_64 and booted (#514).
#
# On this x86_64 build VM: cross-builds the engine (cargo zigbuild), lays out a
# modules tree as stormcos's modules golden has it from Fedora's signed aarch64
# kernel + firmware, and runs scripts/build-stormblock-initramfs.sh with
# nothing but that binary and that tree, as the platform will. Then:
#
#   * every program in the image is aarch64, and it carries r8169, nvme,
#     nvme-tcp, ublk_drv and rtl_nic's rtl8125/8126 firmware;
#   * an x86_64 binary with that kernel is refused;
#   * A: Fedora's kernel under UEFI in ACPI mode (acpi=force, no device tree),
#     as PID 1 the image's real /init: its udev and modprobe find an NVMe drive
#     (the M.2) by themselves, the engine (aarch64) opens the slab on it,
#     exports the root over ublk, switch_root reaches the root's own init;
#   * B: the same with no disk and a NIC: DHCP, the boothost claim over HTTP,
#     the slab over NVMe/TCP from stormboot-serve on this host, root over ublk;
#   * a boot hook loads r8169, nvme_tcp and ublk_drv through the image's own
#     modprobe (dynamically linked, modules compressed).
#
# QEMU emulates no RTL8125, so r8169 loading its firmware at probe waits for the
# board (stormbootx#172); and no CIX P1, so the SoC's own drivers are only known
# to be in the image.
#
#   sc-build 'bash ci-initramfs-aarch64-verify.sh'      (KERNEL=<n-v-r> for one)
set -uo pipefail
ulimit -n "$(ulimit -Hn)"   # zig's linker holds every object open (a 1024 limit: ProcessFdQuotaExceeded)

ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-a64.XXXXXX")
SRV_PID=""
trap '[ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; rm -rf "$W"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }

QEMU=$(command -v qemu-system-aarch64) || { echo "SKIP: no qemu-system-aarch64"; exit 2; }
EFI=/usr/share/edk2/aarch64/QEMU_EFI.fd
[ -r "$EFI" ] || { echo "SKIP: no $EFI"; exit 2; }

say "the engine: aarch64 (zigbuild), and x86_64 to lay the disk and serve it"
cargo zigbuild --release --locked --target aarch64-unknown-linux-musl --bin stormblock 2>&1 | tail -1
cargo build --release --locked --target x86_64-unknown-linux-musl --bin stormblock 2>&1 | tail -1
cargo build --release --locked -p stormblock-nvmeof --bin stormboot-serve 2>&1 | tail -1
T=${CARGO_TARGET_DIR:-$ROOT/target}
BIN="$T/aarch64-unknown-linux-musl/release/stormblock"
HOSTBIN="$T/x86_64-unknown-linux-musl/release/stormblock"
SERVE="$T/release/stormboot-serve"
for b in "$BIN" "$HOSTBIN" "$SERVE"; do [ -x "$b" ] || { echo "FAIL: no $b"; exit 1; }; done
ok "$("$(command -v qemu-aarch64-static || command -v qemu-aarch64)" "$BIN" --version), $(du -h "$BIN" | cut -f1)"

say "a modules tree as the modules golden lays it, from Fedora's signed aarch64 RPMs"
D="$W/dnf"; mkdir -p "$D"/{cache,persist,log} "$W/rpm" "$W/modroot/lib" "$W/keys"
DNF=(dnf -q --releasever=44 --forcearch=aarch64 "--setopt=cachedir=$D/cache"
     "--setopt=persistdir=$D/persist" "--setopt=logdir=$D/log" --setopt=countme=0)
SEL="kernel-core kernel-modules-core kernel-modules"
if [ -n "${KERNEL:-}" ]; then SEL="kernel-core-$KERNEL kernel-modules-core-$KERNEL kernel-modules-$KERNEL"; fi
# shellcheck disable=SC2086
(cd "$W/rpm" && "${DNF[@]}" download $SEL realtek-firmware linux-firmware) || { echo "FAIL: dnf download"; exit 1; }
rpmkeys --dbpath "$W/keys" --import /etc/pki/rpm-gpg/RPM-GPG-KEY-fedora-44-aarch64 || { echo "FAIL: key"; exit 1; }
if rpmkeys --dbpath "$W/keys" -K "$W"/rpm/*.rpm | grep -v 'digests signatures OK' | grep .; then
    echo "FAIL: an RPM is not signed"; exit 1
fi
for r in "$W"/rpm/*.rpm; do
    (cd "$W/modroot" && rpm2cpio "$r" | cpio -idm --quiet --no-absolute-filenames 2>/dev/null)
done
M="$W/modroot"
KVER=$(ls "$M/lib/modules" | tail -1)
# The golden keeps firmware at lib/firmware (stormcos's build-goldens.sh).
mv "$M/usr/lib/firmware" "$M/lib/firmware" || { echo "FAIL: no firmware in the RPMs"; exit 1; }
depmod -b "$M" "$KVER" || { echo "FAIL: depmod over the tree"; exit 1; }
KERNEL_IMG="$M/lib/modules/$KVER/vmlinuz"
ok "kernel $KVER: ACPI $(grep -c '^CONFIG_ACPI=y' "$M/lib/modules/$KVER/config"), ARCH_CIX $(grep -c '^CONFIG_ARCH_CIX=y' "$M/lib/modules/$KVER/config")"

say "refused: another architecture's binary"
if MODROOT="$M" bash scripts/build-stormblock-initramfs.sh "$HOSTBIN" "$KVER" "$W/no.img" > "$W/no.log" 2>&1; then
    fail "an x86_64 engine with an aarch64 kernel built"
elif grep -q "is not for x86_64" "$W/no.log"; then ok "an x86_64 engine with an aarch64 kernel"
else fail "refused for another reason"; tail -5 "$W/no.log"; fi

say "the initramfs, and a boot hook for the modprobe checks"
cat > "$W/10-a64" <<'HOOK'
#!/bin/sh
# Not a decision: exit 1 is "no opinion". What this image's own tools can do.
exec >&2   # a hook's stdout is its answer; its stderr is the console
echo "@@ARCH $(uname -m)"
[ -d /sys/firmware/acpi/tables ] && echo "@@ACPI tables"
dmesg | grep -q 'ACPI: Interpreter enabled' && echo "@@ACPI interpreter"
for m in r8169 nvme_tcp ublk_drv; do
    if modprobe "$m" 2>&1 && [ -d "/sys/module/$m" ]; then echo "@@LOADED $m"
    else echo "@@NOTLOADED $m"; fi
done
echo "@@RTLFW $(ls /lib/firmware/rtl_nic 2>/dev/null | grep -c '^rtl812[56]')"
exit 1
HOOK
if ! MODROOT="$M" BOOT_HOOKS="$W/10-a64" bash scripts/build-stormblock-initramfs.sh "$BIN" "$KVER" "$W/initrd.img" > "$W/initrd.log" 2>&1; then
    grep -E '^ERROR|^  ERROR' "$W/initrd.log"; tail -30 "$W/initrd.log" | grep -v '^rm: '; echo "FAIL: the initramfs did not build"; exit 1
fi
grep -E '^  (arch|applets|modprobe|udev|firmware|modules|boot hook)' "$W/initrd.log" | sed 's/^/  /'
ok "initramfs $(du -h "$W/initrd.img" | cut -f1)"

say "what is in it"
I="$W/unpacked"; mkdir -p "$I"
[ "$(head -c 4 "$W/initrd.img" | od -An -tx1 | tr -d ' ')" = 28b52ffd ] \
    && ok "no early microcode cpio ahead of the archive" || fail "the image does not start with the zstd archive"
(cd "$I" && zstd -dc "$W/initrd.img" | cpio -idm --quiet 2>/dev/null)
n=0; bad=0
while IFS= read -r f; do
    [ "$(head -c 4 "$f" | od -An -c | tr -d ' ')" = '177ELF' ] || continue
    n=$((n + 1))
    [ "$(od -An -tu2 -j18 -N2 "$f" | tr -d ' ')" = 183 ] || { echo "  not aarch64: ${f#"$I"}"; bad=$((bad + 1)); }
done < <(find "$I" -type f ! -path '*/lib/firmware/*' ! -name '*.ko*')
[ "$bad" = 0 ] && ok "$n programs and libraries, all aarch64" || fail "$bad not aarch64"
for f in lib/ld-linux-aarch64.so.1 usr/sbin/modprobe usr/bin/udevadm usr/sbin/stormblock bin/busybox etc/stormblock/userland-packages.tsv; do
    [ -e "$I/$f" ] && ok "/$f" || fail "no /$f"
done
for m in net/ethernet/realtek/r8169 nvme/host/nvme nvme/host/nvme-tcp block/ublk_drv mailbox/cix-mailbox; do
    ls "$I/lib/modules/$KVER/kernel/drivers/$m".ko* >/dev/null 2>&1 && ok "$m" || fail "no $m"
done
grep -q 'pci:v000010ECd00008125' "$I/lib/modules/$KVER/modules.alias" && ok "r8169 answers the RTL8125's PCI id" || fail "no alias for the RTL8125"
for fw in rtl8125 rtl8126; do
    ls "$I/lib/firmware/rtl_nic/$fw"* >/dev/null 2>&1 && ok "rtl_nic/$fw*" || fail "no rtl_nic/$fw*"
done
grep -qE 'device-tree|devicetree' "$I/init" && fail "/init reads the device tree" || ok "/init reads no device tree"

say "a release disk: stormpump's root, with an aarch64 init"
R="$W/root"; mkdir -p "$R"/{bin,sbin,etc,proc,sys,dev,run,tmp}
cp "$I/bin/busybox" "$R/bin/busybox"
for a in sh echo mount uname cat poweroff sleep tr grep; do ln -s busybox "$R/bin/$a"; done
printf 'NAME=stormcos\nPRETTY_NAME="stormcos a64-test"\nVERSION_ID=0\n' > "$R/etc/os-release"
cat > "$R/sbin/init" <<'INIT'
#!/bin/sh
echo "@@APPLIANCE $(cat /run/stormblock/appliance 2>/dev/null || echo none)"
echo "@@ROOT-INIT $(uname -m) $(tr ' ' '\n' < /proc/cmdline | grep -c '^acpi=force$')"
poweroff -f
INIT
chmod 755 "$R/sbin/init"
mkfs.ext4 -q -b 4096 -d "$R" "$W/root.img" 32M || { echo "FAIL: mkfs.ext4"; exit 1; }
truncate -s 16M "$W/blank.img"
cat > "$W/image.toml" <<TOML
name = "a64"
size = "512M"

[data_slab]
size = "96M"
  [[data_slab.golden]]
  name  = "pod-logs"
  file  = "$W/blank.img"
  clone = "pod-logs"

[slab]
size = "rest"
  [[slab.golden]]
  name  = "stormpump"
  file  = "$W/root.img"
  clone = "stormpump"
TOML
(cd "$W" && "$HOSTBIN" image build --spec image.toml --out disk.img > build.log 2>&1) \
    || { tail -20 "$W/build.log"; echo "FAIL: image build"; exit 1; }
ok "disk.img"

boot() { # name cmdline qemu-args... -> $W/<name>.txt; stops at a terminal line
    local name=$1 extra=$2 log="$W/$1.log" t=0 q; shift 2
    "$QEMU" -machine virt,gic-version=3,acpi=on -accel tcg,thread=multi -cpu max,pauth-impdef=on \
        -m 2048 -smp 4 -bios "$EFI" -nographic -no-reboot \
        -kernel "$KERNEL_IMG" -initrd "$W/initrd.img" \
        -append "console=ttyAMA0 acpi=force panic=-1 loglevel=4 root=/dev/ublkb0 stormblock.volume=stormpump $extra" \
        "$@" > "$log" 2>&1 < /dev/null &
    q=$!
    while [ $t -lt 900 ]; do
        sleep 5; t=$((t + 5))
        grep -qE '@@ROOT-INIT|Dropping to shell|Kernel panic' "$log" 2>/dev/null && { sleep 5; break; }
        kill -0 $q 2>/dev/null || break
    done
    kill $q 2>/dev/null; wait $q 2>/dev/null
    tr -d '\r' < "$log" > "$W/$name.txt"
    echo "  ($name: ${t}s)"
}
expect() { # boot needle what
    if grep -qF -- "$2" "$W/$1.txt"; then ok "$1: $3"; else fail "$1: $3 - no '$2'"; tail -60 "$W/$1.txt" | sed 's/^/    | /'; fi
}
checks() { # boot
    expect "$1" "@@ARCH aarch64" "the image runs on aarch64"
    expect "$1" "@@ACPI tables" "/sys/firmware/acpi/tables"
    expect "$1" "@@ACPI interpreter" "the kernel runs in ACPI mode"
    expect "$1" "@@LOADED r8169" "r8169 loads through the image's modprobe"
    expect "$1" "@@LOADED nvme_tcp" "nvme_tcp loads"
    expect "$1" "@@LOADED ublk_drv" "ublk_drv loads"
    expect "$1" "@@ROOT-INIT aarch64 1" "the root's own init ran after switch_root"
}

say "A: UEFI + ACPI, the slab on an NVMe drive, no network"
cp --sparse=always "$W/disk.img" "$W/A.disk"
boot A "rd.stormblock.slab=/dev/nvme0n1" -nic none \
    -drive "id=d0,file=$W/A.disk,format=raw,if=none" -device nvme,serial=o6n-m2,drive=d0
checks A
expect A "@@APPLIANCE none" "no appliance: nothing left in /run/stormblock/appliance (#506)"
expect A "Root device ready: /dev/ublkb0" "the engine serves the root"

say "B: UEFI + ACPI, no disk: DHCP, the claim, the slab over NVMe/TCP"
PORT=$((20000 + RANDOM % 20000)); HTTP=$((PORT + 1))
RUST_LOG=info "$SERVE" --image "$W/disk.img" --host o6n --listen 127.0.0.1 --nvme-port "$PORT" \
    --http-port "$HTTP" --advertise 10.0.2.2 --scratch "$W/overlays" > "$W/serve.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 100); do curl -sf "http://127.0.0.1:$HTTP/api/v1/health" >/dev/null 2>&1 && break; sleep 0.1; done
curl -sf "http://127.0.0.1:$HTTP/api/v1/health" >/dev/null || { echo "FAIL: stormboot-serve did not start"; cat "$W/serve.log"; exit 1; }
boot B "ip=dhcp rd.stormblock.boothost=http://10.0.2.2:$HTTP rd.stormblock.tag=o6n rd.stormblock.diskless=1" \
    -nic "user,model=virtio-net-pci"
checks B
expect B "@@APPLIANCE http://10.0.2.2:$HTTP" "the appliance's URL is there after switch_root (#506)"
expect B "Root device ready: /dev/ublkb0" "the engine serves the root"
grep -q 'nvme-tcp://' "$W/B.txt" && ok "B: the slab named over NVMe/TCP" || fail "B: no nvme-tcp:// on the console"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILS"; exit 1; fi
