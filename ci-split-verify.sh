#!/usr/bin/env bash
# A node on two drives, booted by the shipped initramfs on a real kernel (#402).
#
# tests/initramfs-boot-hook.sh runs the blocks against stubs. This takes the
# two drives `cli::install_tests::the_system_half_moves_to_a_drive_of_its_own_
# and_the_data_stays_where_it_was` leaves (STORMBLOCK_SPLIT_OUT): release N
# installed on one drive, then N+1 installed with the system half on a drive
# of its own (the SSD, 20 GiB) and the data half kept on the first (the HDD),
# flowed over. Then dev's kernel in QEMU, PID 1 = /init, no network:
#
#   A. rd.stormblock.slab= names the SSD (vda), the HDD is vdb: the probe
#      pairs them by slab id and the root comes up;
#   B. the HDD is named and enumerates first (vda): the probe boots through
#      the SSD (vdb), its partner; the root comes up;
#   C. the HDD alone: half a node - no appliance to ask, so the boot stops,
#      naming the missing root volume, and the engine is never started.
#
# Needs: cargo (musl target), qemu-system-x86_64, /boot/vmlinuz-$(uname -r),
# its modules, busybox, mkfs.ext4. Unprivileged.
# Run on dev through sc-build:  sc-build 'bash ci-split-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-split.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }

[ -r "$KERNEL" ] || { echo "SKIP: no $KERNEL"; exit 2; }

say "the two drives, from the split install test"
STORMBLOCK_SPLIT_OUT="$W" cargo test --locked --lib -- --exact \
    cli::install_tests::the_system_half_moves_to_a_drive_of_its_own_and_the_data_stays_where_it_was \
    > "$W/test.log" 2>&1 || { tail -30 "$W/test.log"; echo "FAIL: the split install test"; exit 1; }
[ -s "$W/ssd.raw" ] && [ -s "$W/hdd.raw" ] || { echo "FAIL: the test left no drives"; exit 1; }
ok "ssd.raw $(du -h "$W/ssd.raw" | cut -f1) used, hdd.raw $(du -h "$W/hdd.raw" | cut -f1) used"

say "build the engine (musl) and the initramfs"
cargo build --release --locked --target x86_64-unknown-linux-musl --bin stormblock 2>&1 | tail -2
TDIR=$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
BIN="${TDIR:-$ROOT/target}/x86_64-unknown-linux-musl/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no engine at $BIN"; exit 1; }
bash scripts/build-stormblock-initramfs.sh "$BIN" "$KVER" "$W/initrd.img" > "$W/initrd.log" 2>&1 \
    || { tail -20 "$W/initrd.log"; echo "FAIL: the initramfs did not build"; exit 1; }
ok "initramfs $(du -h "$W/initrd.img" | cut -f1)"
"$BIN" slab partner "$W/hdd.raw" --among "$W/ssd.raw" --among "$W/hdd.raw" 2>&1 | sed 's/^/  /'
"$BIN" slab partner "$W/ssd.raw" --among "$W/ssd.raw" --among "$W/hdd.raw" 2>&1 | sed 's/^/  /'

ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
boot() { # name first-drive second-drive|- -> $W/<name>.txt
    local name=$1 log="$W/$1.log" drives=()
    for d in "$2" "$3"; do
        [ "$d" = - ] && continue
        cp --sparse=always "$W/$d.raw" "$W/$name-$d.raw"
        drives+=(-drive "file=$W/$name-$d.raw,format=raw,if=virtio")
    done
    qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1024 -smp 2 \
        -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.img" \
        "${drives[@]}" -nic none \
        -append "console=ttyS0 panic=-1 loglevel=4 root=/dev/ublkb0 rd.stormblock.slab=/dev/vda stormblock.volume=stormpump rd.stormblock.mount=" \
        > "$log" 2>&1 < /dev/null &
    local q=$! t=0
    while [ $t -lt 300 ]; do
        sleep 2; t=$((t + 2))
        if grep -qE "Dropping to shell|Root device ready" "$log" 2>/dev/null; then sleep 3; break; fi
        kill -0 $q 2>/dev/null || break
    done
    kill $q 2>/dev/null; wait $q 2>/dev/null
    tr -d '\r' < "$log" > "$W/$name.txt"
    rm -f "$W/$name-"*.raw
    echo "  ($name: ${t}s)"
}
has()    { grep -qF -- "$2" "$W/$1.txt"; }
expect() { # boot needle what
    if has "$1" "$2"; then ok "$1: $3"; else fail "$1: $3 - no '$2'"; tail -40 "$W/$1.txt" | sed 's/^/    | /'; fi
}

say "A: the SSD named, the HDD beside it"
boot A ssd hdd
expect A "/dev/vda holds this node's system half, /dev/vdb its data half" "pairs the drives"
expect A "Root device ready: /dev/ublkb0" "the root comes up"

say "B: the HDD named and first"
boot B hdd ssd
expect B "/dev/vda holds this node's data half, /dev/vdb its system half: booting through /dev/vdb" \
    "boots through the system drive"
expect B "Root device ready: /dev/ublkb0" "the root comes up"

say "C: the HDD alone"
boot C hdd -
expect C "has no 'stormpump' volume" "half a node does not boot"
has C "the storage engine (PID" && fail "C: the engine was started on half a node" \
    || ok "C: the engine was never handed the drive"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILS"; exit 1; fi
