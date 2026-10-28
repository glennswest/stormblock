#!/bin/sh
# A boot from a drive this node's system was moved off (#350).
#
# stormdrive's move-system leaves a pointer in the old drive's slab headers to
# the drive the system went to. A machine whose BIOS still boots the old drive
# must boot the moved system, never the stale drive, and say urgently that the
# boot order is wrong; health carries it (boot-order.json). A pointer to a
# drive that is not here falls back to the appliance, said. Runs the real
# block, extracted between its markers from the script this repo generates.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN node halves/,/# --- END node halves/p' "$GEN" > "$WORK/moved.sh"
sed -n '/# --- BEGIN system moved/,/# --- END system moved/p' "$GEN" >> "$WORK/moved.sh"
grep -q 'END system moved' "$WORK/moved.sh" || { echo "FAIL: could not extract the system moved block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

# `stormblock slab moved <disk>`: STUB_MOVED_RC and STUB_MOVED_OUT.
STUB="$WORK/stormblock"
cat > "$STUB" <<'STUBEOF'
#!/bin/sh
if [ "$1 $2" = "slab moved" ]; then
    printf '%s\n' "$STUB_MOVED_OUT"
    exit "$STUB_MOVED_RC"
fi
exit 1
STUBEOF
chmod +x "$STUB"

run() { # slab rc out -> "SLAB|fallback" ; console in $WORK/log
    (
        set +e
        STORM_STORMBLOCK="$STUB"; STORM_RUN="$WORK/run"; export STORM_STORMBLOCK STORM_RUN
        STUB_MOVED_RC="$2"; STUB_MOVED_OUT="$3"; export STUB_MOVED_RC STUB_MOVED_OUT
        rm -rf "$WORK/run"
        HOOK_DECIDED=""; SLAB="$1"; FELL=""
        probe_fallback() { FELL="$1"; SLAB=""; }
        . "$WORK/moved.sh" > "$WORK/log" 2>&1
        echo "$SLAB|${FELL:+fallback}"
    )
}

echo "a boot from a drive the system moved off (#350):"
check "not moved: the drive boots as before" "/dev/sda|" "$(run /dev/sda 1 "/dev/sda: the system did not move off it")"
[ -e "$WORK/run/boot-order.json" ] && check "  no boot-order message" no yes || check "  no boot-order message" no no
check "moved to sdb: the moved system boots" "/dev/sdb|" \
    "$(run /dev/sda2 0 "/dev/sda: the system moved to /dev/sdb (data slab x)
/dev/sdb")"
grep -q "URGENT: SET THE BIOS BOOT ORDER TO /dev/sdb" "$WORK/log" \
    && check "  said urgently" yes yes || check "  said urgently: $(cat "$WORK/log")" yes no
case "$(cat "$WORK/run/boot-order.json" 2>/dev/null)" in
*'"set_to": "/dev/sdb"'*'"booted_from": "/dev/sda"'*'set BIOS boot order to /dev/sdb'*) check "  health's boot-order message written" yes yes ;;
*) check "  health's boot-order message written" yes no ;;
esac
check "moved to a drive not here: the appliance, said" "|fallback" \
    "$(run /dev/sda 2 "/dev/sda: the system moved to data slab x, which is on no drive here")"
grep -q "to a drive that is not on this machine" "$WORK/log" \
    && check "  said urgently" yes yes || check "  said urgently" yes no
check "a claimed image (a URI) is never asked" "nvme-tcp://10.0.0.1:4420/nqn.x|" \
    "$(run nvme-tcp://10.0.0.1:4420/nqn.x 0 "/dev/sdb")"

[ "$fail" -eq 0 ] && echo "PASS" || { echo "FAIL"; exit 1; }
