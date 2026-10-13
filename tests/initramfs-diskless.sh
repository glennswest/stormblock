#!/bin/sh
# Diskless is an error, not a mode (#427; owner on #344: the flow-over onto
# the node's own disk is mandatory).
#
# The Dell R230 ran 12.21 from the appliance with its own sda present, said
# only "no local drive to take; writes stay on the appliance", and hung. Pinned
# here, on the real code (the `diskless refused` block, extracted between its
# markers):
#   * a boot from the appliance's clone that took no local drive stops, with
#     every drive left and why, and the way to run diskless on purpose;
#   * rd.stormblock.assimilate=off and rd.stormblock.diskless=1 run it, said;
#   * a drive taken, or a boot from a local disk, goes on with no word.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN diskless refused/,/# --- END diskless refused/p' "$GEN" > "$WORK/diskless.sh"
[ -s "$WORK/diskless.sh" ] || { echo "FAIL: could not extract the diskless refused block"; exit 1; }

fail=0
has() { # name needle haystack
    case "$3" in
    *"$2"*) echo "  ok    $1" ;;
    *) echo "  FAIL  $1: '$2' not in: $3"; fail=1 ;;
    esac
}
hasnt() {
    case "$3" in
    *"$2"*) echo "  FAIL  $1: '$2' in: $3"; fail=1 ;;
    *) echo "  ok    $1" ;;
    esac
}

run() { # slab local-disk assimilate diskless-ok skips ld-drive
    SLAB="$1" LOCAL_DISK="$2" ASSIMILATE="$3" DISKLESS_OK="$4" LD_SKIPS="$5" LD_DRIVE="$6" \
        LD_WHY="carries stormblock slabs that rd.stormblock.assimilate=any does not take" LD_ODRIVE="" sh -c '
        rescue_shell() { echo RESCUE; exit 0; }
        ld_write() { echo "VERDICT $1 $2 $3"; }
        . "$0"
        echo ON
    ' "$WORK/diskless.sh" 2>&1
}

REMOTE="nvme-tcp://192.168.30.252:4420/nqn.2026-08.lo.storm:forge:host:C2NR0Q2?nsid=1"
SKIPS="
    /dev/sda: carries stormblock slabs that rd.stormblock.assimilate=any does not take
    /dev/sdb: removable"

echo "diskless by default:"
out=$(run "$REMOTE" "" any "" "$SKIPS" /dev/sda)
has "it stops" "RESCUE" "$out"
hasnt "and does not go on" "ON" "$out"
has "said as FATAL" "FATAL: this node would run diskless from the appliance" "$out"
has "every drive left is named, with why" "/dev/sda: carries stormblock slabs" "$out"
has "the removable one too" "/dev/sdb: removable" "$out"
has "the way to run diskless on purpose" "rd.stormblock.diskless=1" "$out"
has "the verdict for the engine's health" "VERDICT failed /dev/sda diskless refused" "$out"
out=$(run "$REMOTE" "" any "" "" "")
has "no drive at all: stops too, and says none was seen" "No drive was seen" "$out"
has "stops" "RESCUE" "$out"

echo "on purpose:"
out=$(run "$REMOTE" "" off "" "$SKIPS" /dev/sda)
has "assimilate=off runs it" "ON" "$out"
has "said" "DISKLESS, as rd.stormblock.assimilate=off asks" "$out"
out=$(run "$REMOTE" "" any 1 "$SKIPS" /dev/sda)
has "diskless=1 runs it" "ON" "$out"
has "said" "DISKLESS, as rd.stormblock.diskless=1 asks" "$out"

echo "not diskless:"
out=$(run "$REMOTE" /dev/sda any "" "" "")
has "a drive taken: on" "ON" "$out"
hasnt "with no word" "DISKLESS" "$out"
out=$(run /dev/sda4 "" any "" "
    /dev/sda: this boot runs from it" "")
has "a boot from the local disk: on" "ON" "$out"
hasnt "never FATAL" "FATAL" "$out"

if command -v busybox >/dev/null 2>&1; then
    busybox sh -n "$WORK/diskless.sh" && echo "  ok    parses under busybox sh" \
        || { echo "  FAIL  busybox sh -n"; fail=1; }
fi

[ "$fail" = 0 ] && echo "PASS" || { echo "FAIL"; exit 1; }
