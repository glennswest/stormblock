#!/bin/sh
# The container volumes are mounted in parallel (#302).
#
# They were mounted one at a time, ~130-200 ms each: 8.6-10.5 s of every boot
# for 63 volumes (stormcos#300). Runs the real block, extracted from the init
# script this repo generates between its marker comments, with `mount` and the
# device test stubbed: a stub mount that takes STUB_MS, logs when each mount
# starts and ends, and records how many run at once.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN container mounts/,/# --- END container mounts/p' "$GEN" > "$WORK/block.sh"
[ -s "$WORK/block.sh" ] || { echo "FAIL: could not extract the container mounts block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}
ok() { # name condition-result(0/1)
    if [ "$2" = 0 ]; then echo "  ok    $1"; else echo "  FAIL  $1"; fail=1; fi
}

# mount [-t type] dev dir. XFS devices (named in STUB_XFS) refuse -t ext4;
# devices named in STUB_BAD refuse everything.
STUB="$WORK/mount"
cat > "$STUB" <<'STUBEOF'
#!/bin/sh
t=""
if [ "$1" = "-t" ]; then t="$2"; shift 2; fi
dev="$1"; dir="$2"
name=${dev##*/}
echo "call ${t:-probe} $name $dir" >> "$STUB_LOG"
case " ${STUB_BAD:-} " in *" $name "*) [ -e "$STUB_MENDED/$name" ] || exit 32 ;; esac
case " ${STUB_XFS:-} " in *" $name "*) [ "$t" = ext4 ] && exit 32 ;; esac
mkdir -p "$STUB_RUNNING"
touch "$STUB_RUNNING/$$"
n=$(ls "$STUB_RUNNING" | wc -l)
echo "$n" >> "$STUB_CONC"
echo "start $dir" >> "$STUB_LOG"
sleep "$STUB_SLEEP"
echo "end $dir" >> "$STUB_LOG"
rm -f "$STUB_RUNNING/$$"
exit 0
STUBEOF
chmod +x "$STUB"

now() { cut -d' ' -f1 /proc/uptime; }

# stormblock ext4-repair, stubbed (#532): devices in STUB_MENDS are mended
# (the mount stub then takes them); fake:NAME says repaired and mends nothing;
# any other answers as the engine does for one it cannot repair.
REPAIR="$WORK/repair"
cat > "$REPAIR" <<'STUBEOF'
#!/bin/sh
name=${1##*/}
echo "repair $name" >> "$STUB_LOG"
case " ${STUB_MENDS:-} " in
    *" $name "*)
        echo "FINDING: $1: 32 of the 32 orphan file block(s) rewritten"
        touch "$STUB_MENDED/$name"
        exit 0 ;;
    *" fake:$name "*)
        echo "$1: repaired"
        exit 0 ;;
esac
echo "$1: 3 problem(s) left; not repaired"
exit 1
STUBEOF
chmod +x "$REPAIR"
: > "$WORK/cmdline"

# run MAP [env...] -> output in $WORK/out, elapsed seconds in $ELAPSED
run() {
    map=$1; shift
    : > "$WORK/log"; : > "$WORK/conc"; rm -rf "$WORK/running" "$WORK/run"
    [ -n "${KEEP_SYSROOT:-}" ] || rm -rf "$WORK/sysroot"
    mkdir -p "$WORK/run" "$WORK/mended"
    rm -f "${WORK:?}"/mended/*
    t0=$(now)
    env STUB_LOG="$WORK/log" STUB_CONC="$WORK/conc" STUB_RUNNING="$WORK/running" STUB_MENDED="$WORK/mended" \
        STORM_CMDLINE="$WORK/cmdline" STORM_MOUNT_REPAIR="$REPAIR" \
        STUB_SLEEP="${SLEEP:-0.3}" STORM_MOUNT="$STUB" STORM_DEV_TEST=-e \
        STORM_SYSROOT="$WORK/sysroot" STORM_RUN="$WORK/run" "$@" \
        MOUNT_MAP="$map" ${SHELL_UNDER_TEST:-sh} -c '. "$1"' sh "$WORK/block.sh" > "$WORK/out" 2>&1
    t1=$(now)
    ELAPSED=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", b - a }')
}

DEV="$WORK/dev"
mkdir -p "$DEV"
MAP=""
i=0
while [ $i -lt 20 ]; do
    : > "$DEV/ublkb$i"
    MAP="$MAP$DEV/ublkb$i /c/vol$i
"
    i=$((i + 1))
done

for sh in sh "busybox sh"; do
    if [ "$sh" = "busybox sh" ] && ! command -v busybox >/dev/null; then
        echo "  skip  busybox sh: no busybox"
        continue
    fi
    echo "== under $sh"
    SHELL_UNDER_TEST=$sh

    # 20 volumes, 0.3 s each: 6 s one at a time; two batches of 16 here.
    run "$MAP"
    check "every volume mounted" 20 "$(grep -c '  mounted: ' "$WORK/out")"
    check "-t ext4 first, nothing probed" 20 "$(grep -c '^call ext4 ' "$WORK/log")"
    ok "in parallel: $ELAPSED s, not 6" "$(awk -v e="$ELAPSED" 'BEGIN { exit !(e < 2.5) }'; echo $?)"
    check "bounded at 16" 16 "$(sort -n "$WORK/conc" | tail -1)"
    ok "the count and the time said" "$(grep -q '20 volume(s) mounted in .* s (16 at a time)' "$WORK/out"; echo $?)"
    check "mount points made" 20 "$(ls "$WORK/sysroot/c" | wc -l | tr -d ' ')"

    # A smaller bound is kept.
    run "$MAP" STORM_MOUNT_PARALLEL=4
    check "STORM_MOUNT_PARALLEL=4 bounds it" 4 "$(sort -n "$WORK/conc" | tail -1)"

    # Nested: a mount point inside another is mounted after its parent.
    : > "$DEV/ublkb40"; : > "$DEV/ublkb41"; : > "$DEV/ublkb42"
    run "$DEV/ublkb42 /data/sub/deeper
$DEV/ublkb41 /data/sub
$DEV/ublkb40 /data
"
    order=$(grep -E '^(start|end) ' "$WORK/log" | sed "s|$WORK/sysroot||" | tr '\n' ' ')
    check "parent before child before grandchild" \
        "start /data end /data start /data/sub end /data/sub start /data/sub/deeper end /data/sub/deeper " "$order"

    # XFS: -t ext4 refused, the probing mount takes it.
    run "$DEV/ublkb0 /c/x0
$DEV/ublkb1 /c/x1
" STUB_XFS=ublkb1
    check "an XFS volume falls back to probing" "call probe ublkb1 /tmp" \
        "$(grep '^call probe ' "$WORK/log" | sed "s|$WORK/sysroot/c/x1|/tmp|")"
    check "and is mounted" 2 "$(grep -c '  mounted: ' "$WORK/out")"

    # A volume that will not mount is warned, the rest are mounted.
    run "$DEV/ublkb0 /c/b0
$DEV/ublkb1 /c/b1
" STUB_BAD=ublkb1
    check "a bad volume warned" 1 "$(grep -c 'WARNING: .*ublkb1 would not mount at /c/b1' "$WORK/out")"
    check "the others mounted" 1 "$(grep -c '  mounted: ' "$WORK/out")"

    # #532: nothing refused, nothing repaired, nothing reported.
    run "$DEV/ublkb0 /c/ok0
"
    check "a good mount: no repair run" 0 "$(grep -c '^repair ' "$WORK/log")"
    check "a good mount: nothing reported" "" "$(cat "$WORK/run/stormblock/boot-mounts")"
    check "a good mount: no FATAL" 0 "$(grep -c FATAL "$WORK/out")"

    # #532: refused, repaired, mounted, and said so; the others untouched.
    run "$DEV/ublkb0 /c/g0
$DEV/ublkb1 /c/b1
" STUB_BAD=ublkb1 STUB_MENDS=ublkb1
    check "refused: the repair ran on it alone" "repair ublkb1" "$(grep '^repair ' "$WORK/log")"
    check "refused: the finding on the console" 1 "$(grep -c '  FINDING: .*ublkb1' "$WORK/out")"
    check "refused: mounted after the repair" 1 "$(grep -c 'mounted: .*ublkb1 -> /c/b1 (after a repair)' "$WORK/out")"
    check "refused: reported as repaired" 1 "$(grep -c '^repaired .*ublkb1 /c/b1 ' "$WORK/run/stormblock/boot-mounts")"
    check "refused: not a failure" 0 "$(grep -c FATAL "$WORK/out")"

    # #532: a repair that cannot: loud, and in the report with what it said.
    run "$DEV/ublkb0 /c/g0
$DEV/ublkb1 /c/b1
" STUB_BAD=ublkb1
    check "unrepairable: FATAL, counted" 1 "$(grep -c '^FATAL: 1 volume(s) of the mount list are not mounted' "$WORK/out")"
    check "unrepairable: named under it" 1 "$(grep -c "^  $DEV/ublkb1 /c/b1 " "$WORK/out")"
    check "unrepairable: the repair's words kept" 1 \
        "$(grep -c '^failed .*ublkb1 /c/b1 the kernel refused it and it could not be repaired: .*3 problem(s) left' "$WORK/run/stormblock/boot-mounts")"
    check "unrepairable: the other still mounted" 1 "$(grep -c '  mounted: .*ublkb0' "$WORK/out")"

    # #532: the repair says done, the kernel still refuses.
    run "$DEV/ublkb1 /c/b1
" STUB_BAD=ublkb1 STUB_MENDS=fake:ublkb1
    check "refused again: failed" 1 "$(grep -c '^failed .* the kernel refused it again after a repair' "$WORK/run/stormblock/boot-mounts")"

    # #532: off by the command line: never run, and said.
    echo "ro rd.stormblock.mount-repair=off quiet" > "$WORK/cmdline"
    run "$DEV/ublkb1 /c/b1
" STUB_BAD=ublkb1 STUB_MENDS=ublkb1
    : > "$WORK/cmdline"
    check "off: no repair" 0 "$(grep -c '^repair ' "$WORK/log")"
    check "off: failed, saying why" 1 "$(grep -c '^failed .*repair is off' "$WORK/run/stormblock/boot-mounts")"

    # #532: a device that never appeared is in the report too (#522).
    SLEEP=0.1 run "$DEV/nope1 /c/m1
" STORM_MOUNT_WAIT=0
    check "never appeared: reported" 1 "$(grep -c '^failed .*nope1 /c/m1 the device never appeared' "$WORK/run/stormblock/boot-mounts")"
    check "never appeared: FATAL" 1 "$(grep -c '^FATAL: 1 volume' "$WORK/out")"

    # A read-only root (stormcos#470): a mount point the image has is used;
    # a missing one cannot be made, and is named.
    if [ "$(id -u)" != 0 ]; then
        rm -rf "$WORK/sysroot"; mkdir -p "$WORK/sysroot/c/have"; chmod 555 "$WORK/sysroot/c" "$WORK/sysroot"
        KEEP_SYSROOT=1 run "$DEV/ublkb0 /c/have
$DEV/ublkb1 /c/missing
"
        chmod -R u+w "$WORK/sysroot"
        check "read-only root: a missing mount point is named" 1 \
            "$(grep -c 'WARNING: .*ublkb1 would not mount at /c/missing: no such mount point' "$WORK/out")"
        check "read-only root: an existing one is mounted" 1 "$(grep -c '  mounted: .*ublkb0' "$WORK/out")"
    fi

    # Two missing devices: waited for once (2 s), not once each.
    SLEEP=0.1 run "$DEV/ublkb0 /c/m0
$DEV/nope1 /c/m1
$DEV/nope2 /c/m2
" STORM_MOUNT_WAIT=2
    check "missing devices warned" 2 "$(grep -c 'never appeared; /c/m' "$WORK/out")"
    check "the present one mounted" 1 "$(grep -c '  mounted: ' "$WORK/out")"
    ok "one wait for the list: $ELAPSED s" "$(awk -v e="$ELAPSED" 'BEGIN { exit !(e >= 1.9 && e < 3.5) }'; echo $?)"

    # A device that appears during the wait is mounted.
    SLEEP=0.1 run "$DEV/late /c/late
" STORM_MOUNT_WAIT=5 &
    sleep 1; : > "$DEV/late"; wait $!
    check "a late device mounted" 1 "$(grep -c '  mounted: .*late -> /c/late' "$WORK/out")"
    rm -f "$DEV/late"

    # Nothing in the list: nothing said, nothing run.
    run ""
    check "an empty list does nothing" "" "$(cat "$WORK/out")"
done

[ "$fail" = 0 ] && echo "ALL PASS" || { echo "FAILURES"; exit 1; }
