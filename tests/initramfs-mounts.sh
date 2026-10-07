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
if [ "$1 $2" = "slab volumes" ]; then
    [ -n "${STUB_VOLS:-}" ] && [ -r "$STUB_VOLS" ] && cat "$STUB_VOLS"
    exit 0
fi
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

# Optional entries (#288): `?vol:path` mounted when the slab has the volume.
opt() { # mounts vols-file-or-empty slab -> "MOUNTS|MOUNTS_SKIPPED|listings" ; console in $WORK/opt.out
    : > "$WORK/log"
    out=$(STUB_LOG="$WORK/log" STUB_VOLS="$2" STORM_STORMBLOCK="$STUB" STORM_RUN="$WORK/run" \
        MOUNTS="$1" SLAB_ARG="$3" sh -c '
        . "$0"
        mounts_optional "$SLAB_ARG" > "$1"
        printf "%s|%s" "$MOUNTS" "$MOUNTS_SKIPPED"
    ' "$WORK/mounts.sh" "$WORK/opt.out")
    printf '%s|%s' "$out" "$(grep -c 'slab volumes' "$WORK/log" | tr -d ' ')"
}
cat > "$WORK/vols" <<'VOLS'
/dev/sda2: volume stormpump (4.3 GB, 4096 slots, sealed)
/dev/sda2: volume cilium (210 MB, 200 slots, sealed)
/dev/sda2: volume cilium-data (64 MB, 3 slots)
/dev/sda3: volume kubelet-data (2.1 GB, 540 slots)
VOLS
check "no optional entry: the list as it is, and no listing read" \
    "stormblock:/p/stormblock,fastetcd:/p/fastetcd||0" \
    "$(opt "stormblock:/p/stormblock,fastetcd:/p/fastetcd" "$WORK/vols" /dev/sda)"
check "an optional entry the slab has is mounted, in its place" \
    "stormblock:/p/stormblock,cilium:/p/cilium,kubelet-data:/var/lib/kubelet||1" \
    "$(opt "stormblock:/p/stormblock,?cilium:/p/cilium,kubelet-data:/var/lib/kubelet" "$WORK/vols" /dev/sda)"
check "one it does not have is left out, the rest kept in order" \
    "stormblock:/p/stormblock,cilium:/p/cilium|flowsdn:/p/flowsdn,release:/release|1" \
    "$(opt "stormblock:/p/stormblock,?flowsdn:/p/flowsdn,?cilium:/p/cilium,?release:/release" "$WORK/vols" /dev/sda)"
case "$(cat "$WORK/opt.out")" in
*"optional, not in this release: flowsdn (/p/flowsdn)"*) echo "  ok    and the console says which, and why" ;;
*) echo "  FAIL  console: $(cat "$WORK/opt.out")"; fail=1 ;;
esac
check "a name that is a prefix of one the slab has is not taken for it" \
    "|cilium-d:/p/x|1" \
    "$(opt "?cilium-d:/p/x" "$WORK/vols" /dev/sda)"
check "a required entry is never dropped, even when the slab lacks it" \
    "flowsdn:/p/flowsdn|nothing:/n|1" \
    "$(opt "flowsdn:/p/flowsdn,?nothing:/n" "$WORK/vols" /dev/sda)"
echo "/dev/sdb: slab 1111 keeps no volume metadata" > "$WORK/novols"
check "a slab that cannot list its volumes: optional entries left out" \
    "stormblock:/p/stormblock|cilium:/p/cilium|1" \
    "$(opt "stormblock:/p/stormblock,?cilium:/p/cilium" "$WORK/novols" /dev/sdb)"
case "$(cat "$WORK/opt.out")" in
*"optional, left out: cilium (/p/cilium): /dev/sdb cannot list its volumes"*) echo "  ok    and says it could not tell" ;;
*) echo "  FAIL  console: $(cat "$WORK/opt.out")"; fail=1 ;;
esac
check "every entry optional and absent: an empty list" \
    "|flowsdn:/p/flowsdn|1" \
    "$(opt "?flowsdn:/p/flowsdn" "$WORK/vols" /dev/sda)"

# Read from the file, `?` lines survive the comment and space stripping.
cat > "$WORK/optlist" <<'LIST'
stormblock:/p/stormblock
? cilium:/p/cilium      # cilium flavor only
?flowsdn:/p/flowsdn
LIST
check "a ? line in /etc/stormblock/mounts reads as an optional entry" \
    "stormblock:/p/stormblock,?cilium:/p/cilium,?flowsdn:/p/flowsdn|/etc/stormblock/mounts in stormpump|1" \
    "$(run "" "$WORK/optlist" "" /dev/sda)"

# The block parses under the shell /init runs in.
if command -v busybox >/dev/null 2>&1; then
    busybox sh -n "$WORK/mounts.sh" && echo "  ok    parses under busybox sh" \
        || { echo "  FAIL  busybox sh -n"; fail=1; }
fi

[ "$fail" = 0 ] && echo "PASS" || { echo "FAIL"; exit 1; }
