#!/bin/sh
# Where /init gets its mount list (#262).
#
# The list used to be the kernel command line alone, `rd.stormblock.mount=`,
# one ~1.8 KB word on a line x86 caps at 2048 bytes: 11.68 truncated it away
# and mounted nothing. It now comes from the root volume's
# /etc/stormblock/mounts, read with `stormblock slab cat`; the command line
# still works and wins when it is there.
#
# Runs the real code: the block is extracted from the init script this repo
# generates, between its marker comments, so the test cannot drift.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN mount list/,/# --- END mount list/p' "$GEN" > "$WORK/mounts.sh"
[ -s "$WORK/mounts.sh" ] || { echo "FAIL: could not extract the mount list block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

# A stormblock that answers `slab cat` from $STUB_FILE (exit 1 without one),
# and records what it was asked.
STUB="$WORK/stormblock"
cat > "$STUB" <<'STUBEOF'
#!/bin/sh
echo "$*" >> "$STUB_LOG"
[ "$1 $2" = "slab cat" ] || exit 2
out=""; vol=""; slab=""
shift 2
while [ $# -gt 0 ]; do
    case "$1" in
    --out) out="$2"; shift 2 ;;
    --volume) vol="$2"; shift 2 ;;
    --slab) slab="$2"; shift 2 ;;
    *) path="$1"; shift ;;
    esac
done
[ -n "${STUB_FILE:-}" ] && [ -r "$STUB_FILE" ] || exit 1
cp "$STUB_FILE" "$out"
STUBEOF
chmod +x "$STUB"

run() { # cmdline-mounts file-or-empty volume slab -> "MOUNTS|MOUNTS_FROM|calls"
    : > "$WORK/log"
    out=$(STUB_LOG="$WORK/log" STUB_FILE="$2" STORM_STORMBLOCK="$STUB" STORM_RUN="$WORK/run" \
        MOUNTS="$1" VOLUME="$3" SLAB_ARG="$4" sh -c '
        . "$0"
        mounts_from "$SLAB_ARG"
        printf "%s|%s" "$MOUNTS" "$MOUNTS_FROM"
    ' "$WORK/mounts.sh")
    printf '%s|%s' "$out" "$(wc -l < "$WORK/log" | tr -d ' ')"
}

cat > "$WORK/list" <<'LIST'
# what 11.79's root mounts
stormblock:/p/stormblock

fastetcd:/p/fastetcd   # the datastore
fastetcd-data:/d/fastetcd
LIST

check "the list from the root volume's /etc/stormblock/mounts" \
    "stormblock:/p/stormblock,fastetcd:/p/fastetcd,fastetcd-data:/d/fastetcd|/etc/stormblock/mounts in stormpump|1" \
    "$(run "" "$WORK/list" "" /dev/sda)"
check "the root volume named by stormblock.volume=" \
    "stormblock:/p/stormblock,fastetcd:/p/fastetcd,fastetcd-data:/d/fastetcd|/etc/stormblock/mounts in root-v2|1" \
    "$(run "" "$WORK/list" root-v2 'nvme-tcp://10.0.0.1:4420/nqn.x?nsid=1')"
check "the command line still works, and wins: the volume is not read" \
    "a:/p/a,b:/p/b|the command line|0" \
    "$(run "a:/p/a,b:/p/b" "$WORK/list" "" /dev/sda)"
check "neither: no list, and the boot goes on" \
    "||1" \
    "$(run "" "" "" /dev/sda)"
check "no slab to read: no list" \
    "||0" \
    "$(run "" "$WORK/list" "" "")"
: > "$WORK/empty"
check "an empty file: no list" \
    "|/etc/stormblock/mounts in stormpump|1" \
    "$(run "" "$WORK/empty" "" /dev/sda)"

# The block parses under the shell /init runs in.
if command -v busybox >/dev/null 2>&1; then
    busybox sh -n "$WORK/mounts.sh" && echo "  ok    parses under busybox sh" \
        || { echo "  FAIL  busybox sh -n"; fail=1; }
fi

[ "$fail" = 0 ] && echo "PASS" || { echo "FAIL"; exit 1; }
