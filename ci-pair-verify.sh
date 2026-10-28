#!/usr/bin/env bash
# A system pair and a moved system, booted by the shipped initramfs on a real
# kernel (#350).
#
# tests/initramfs-boot-hook.sh and tests/initramfs-system-moved.sh run the
# blocks against stubs. This takes the drives two install tests leave
# (STORMBLOCK_PAIR_OUT): `a_system_pair_installs_onto_two_drives_and_either_
# boots_alone` (release N, then N+1, installed onto a pair, every volume
# mirrored across both) and `the_system_moves_to_another_drive_and_the_old_
# one_points_to_it` (a node moved off its drive). Then the build VM's kernel in
# QEMU, PID 1 = /init, no network:
#
#   A. the pair, drive a first: the probe finds b as a's pair, the root comes up;
#   B. the pair, drive b first: the same through b;
#   C. drive a alone: DEGRADED said, the root comes up from a;
#   D. drive b alone: the same from b;
#   E. the moved-off drive first, the new one beside it: the pointer followed,
#      SET THE BIOS BOOT ORDER said, the root comes up from the new drive.
#
# Needs: cargo (musl target), qemu-system-x86_64, /boot/vmlinuz-$(uname -r),
# its modules, busybox, mkfs.ext4. Unprivileged.
# Run on a build VM:  sc-build 'bash ci-pair-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-pair.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }

[ -r "$KERNEL" ] || { echo "SKIP: no $KERNEL"; exit 2; }

say "the drives, from the pair and move install tests"
STORMBLOCK_PAIR_OUT="$W" cargo test --locked --lib -- \
    cli::install_tests::a_system_pair_installs_onto_two_drives_and_either_boots_alone \
    cli::install_tests::the_system_moves_to_another_drive_and_the_old_one_points_to_it \
    > "$W/test.log" 2>&1 || { tail -40 "$W/test.log"; echo "FAIL: the install tests"; exit 1; }
for d in pair-a pair-b moved-from moved-to; do
    [ -s "$W/$d.raw" ] || { echo "FAIL: the tests left no $d.raw"; exit 1; }
done
ok "pair-a, pair-b, moved-from, moved-to"

say "build the engine (musl) and the initramfs"
cargo build --release --locked --target x86_64-unknown-linux-musl --bin stormblock 2>&1 | tail -2
TDIR=$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
BIN="${TDIR:-$ROOT/target}/x86_64-unknown-linux-musl/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no engine at $BIN"; exit 1; }
bash scripts/build-stormblock-initramfs.sh "$BIN" "$KVER" "$W/initrd.img" > "$W/initrd.log" 2>&1 \
    || { tail -20 "$W/initrd.log"; echo "FAIL: the initramfs did not build"; exit 1; }
ok "initramfs $(du -h "$W/initrd.img" | cut -f1)"
"$BIN" slab partner "$W/pair-a.raw" --among "$W/pair-a.raw" --among "$W/pair-b.raw" 2>&1 | sed 's/^/  /'
"$BIN" slab moved "$W/moved-from.raw" --among "$W/moved-from.raw" --among "$W/moved-to.raw" 2>&1 | sed 's/^/  /'

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

say "A: the pair, a first"
boot A pair-a pair-b
expect A "/dev/vda and /dev/vdb are this node's system pair" "finds the pair"
expect A "Root device ready: /dev/ublkb0" "the root comes up"

say "B: the pair, b first"
boot B pair-b pair-a
expect B "/dev/vda and /dev/vdb are this node's system pair" "finds the pair from the other drive"
expect B "Root device ready: /dev/ublkb0" "the root comes up"

for c in "C pair-a" "D pair-b"; do
    set -- $c
    say "$1: $2 alone"
    boot "$1" "$2" -
    expect "$1" "SYSTEM PAIR DEGRADED" "a missing drive is said"
    expect "$1" "Root device ready: /dev/ublkb0" "the root comes up from one drive"
done

say "E: the moved-off drive first"
boot E moved-from moved-to
expect E "URGENT: SET THE BIOS BOOT ORDER TO /dev/vdb" "the boot order is asked for"
expect E "Root device ready: /dev/ublkb0" "the moved system's root comes up"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILS"; exit 1; fi
