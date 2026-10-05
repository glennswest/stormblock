#!/usr/bin/env bash
# The initramfs with no appliance, on a real kernel, as PID 1 (#294).
#
# tests/initramfs-no-appliance.sh runs the blocks against stubs. This builds
# the shipped initramfs (scripts/build-stormblock-initramfs.sh, this commit's
# engine), lays a node disk with `stormblock image build` - a system slab
# whose `stormpump` (ext4, /etc/os-release "stormcos 11.79-test") and a data
# slab whose `pod-logs` - and boots dev's kernel in QEMU with that disk and
# no network at all, so no appliance can answer:
#
#   A. the command line mounts kubelet-data, which the disk does not have
#      (server8 on 11.82): /init stops with a FATAL naming kubelet-data and
#      the release on the disk - not an engine exit that scrolls away;
#   B. a disk that can boot, and an engine that fails after the probe
#      (rd.stormblock.image-store= names no volume): the console says the
#      release check was skipped and why, and after `FATAL: root device` it
#      repeats the engine's last lines, its error among them;
#   C. a disk that can boot: the engine runs from the log, the root device
#      appears.
#
# Needs: cargo (musl target), qemu-system-x86_64, /boot/vmlinuz-$(uname -r),
# its modules, busybox, mkfs.ext4. Unprivileged.
# Run on dev through sc-build:  sc-build 'bash ci-no-appliance-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-noapp.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }

[ -r "$KERNEL" ] || { echo "SKIP: no $KERNEL"; exit 2; }

say "build the engine (musl) and the initramfs"
cargo build --release --locked --target x86_64-unknown-linux-musl --bin stormblock 2>&1 | tail -2
BIN="$ROOT/target/x86_64-unknown-linux-musl/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no engine at $BIN"; exit 1; }
bash scripts/build-stormblock-initramfs.sh "$BIN" "$KVER" "$W/initrd.img" > "$W/initrd.log" 2>&1 \
    || { tail -20 "$W/initrd.log"; echo "FAIL: the initramfs did not build"; exit 1; }
ok "initramfs $(du -h "$W/initrd.img" | cut -f1)"

say "lay a node disk: stormpump (an older release) and pod-logs, no kubelet-data"
mkdir -p "$W/root/etc"
printf 'NAME=stormcos\nPRETTY_NAME="stormcos 11.79-test"\nVERSION_ID=11.79\n' > "$W/root/etc/os-release"
mkfs.ext4 -q -b 4096 -d "$W/root" "$W/root.img" 32M || { echo "FAIL: mkfs.ext4 -d"; exit 1; }
truncate -s 16M "$W/blank.img"
cat > "$W/image.toml" <<EOF
name = "noapp"
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
EOF
(cd "$W" && "$BIN" image build --spec image.toml --out disk.img > build.log 2>&1) \
    || { tail -20 "$W/build.log"; echo "FAIL: image build"; exit 1; }
"$BIN" slab volumes "$W/disk.img" 2>&1 | sed 's/^/  /' | head -8

ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
boot() { # name extra-cmdline -> $W/<name>.txt; stops at a terminal line
    local name=$1 extra=$2 log="$W/$1.log"
    cp --sparse=always "$W/disk.img" "$W/$name.disk"
    qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1024 -smp 2 \
        -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.img" \
        -drive "file=$W/$name.disk,format=raw,if=virtio" -nic none \
        -append "console=ttyS0 panic=-1 loglevel=4 root=/dev/ublkb0 rd.stormblock.slab=/dev/vda stormblock.volume=stormpump $extra" \
        > "$log" 2>&1 < /dev/null &
    local q=$! t=0
    while [ $t -lt 300 ]; do
        sleep 2; t=$((t + 2))
        if grep -qE "Dropping to shell|Root device ready" "$log" 2>/dev/null; then sleep 3; break; fi
        kill -0 $q 2>/dev/null || break
    done
    kill $q 2>/dev/null; wait $q 2>/dev/null
    tr -d '\r' < "$log" > "$W/$name.txt"
    echo "  ($name: ${t}s)"
}
has()    { grep -qF -- "$2" "$W/$1.txt"; }
expect() { # boot needle what
    if has "$1" "$2"; then ok "$1: $3"; else fail "$1: $3 - no '$2'"; tail -40 "$W/$1.txt" | sed 's/^/    | /'; fi
}

say "A: the disk lacks a volume the release mounts, and there is no appliance"
boot A "rd.stormblock.mount=kubelet-data:/var/lib/kubelet,pod-logs:/var/log/pods"
expect A "No appliance:" "says there is no appliance"
expect A "FATAL: /dev/vda has 'stormpump' but is missing 1 mounted volume(s)" "stops on the missing volume"
expect A "missing: kubelet-data" "names it"
expect A "holds: stormcos 11.79-test" "names the release on the disk"
has A "the storage engine (PID" && fail "A: the engine was started on a disk that cannot boot" \
    || ok "A: the engine was never handed the disk"

say "B: a disk that boots, an engine that fails"
boot B "rd.stormblock.mount=pod-logs:/var/log/pods rd.stormblock.image-store=no-such-volume"
expect B "RELEASE CHECK SKIPPED" "says the release check was skipped"
expect B "FATAL: root device /dev/ublkb0 not found" "the root wait fails"
expect B "The storage engine's last" "repeats the engine's output after it"
if sed -n "/The storage engine's last/,\$p" "$W/B.txt" | grep -q "^  | .*no-such-volume"; then
    ok "B: the engine's error is in what is repeated"
else
    fail "B: the engine's error is not repeated after the FATAL"
    sed -n "/FATAL: root device/,\$p" "$W/B.txt" | head -40 | sed 's/^/    | /'
fi

say "C: a disk that boots"
boot C "rd.stormblock.mount=pod-logs:/var/log/pods"
expect C "RELEASE CHECK SKIPPED" "says the release check was skipped"
expect C "Root device ready: /dev/ublkb0" "the root device appears"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILS"; exit 1; fi
