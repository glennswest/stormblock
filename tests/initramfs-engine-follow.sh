#!/bin/sh
# The engine follower: a flood of same-shaped lines reaches the console
# three times and a count, never thousands of lines (#476).
#
# server8's install wrote 4822 per-extent warnings into the engine log in a
# tenth of a second; the follower put every one on a console that showed
# three a second, and /init's own lines waited behind them for half an hour.
# Runs the real code: the block is extracted from the init script this repo
# generates, between its two marker comments.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
mkdir -p "$HERE/../tmp"
WORK=$(mktemp -d "$HERE/../tmp/follow.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN engine follower/,/# --- END engine follower/p' "$GEN" > "$WORK/follow.sh"
[ -s "$WORK/follow.sh" ] || { echo "FAIL: could not extract the engine follower"; exit 1; }
ENGINE_LOG=/run/stormblock/engine.log
. "$WORK/follow.sh"

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

# The flood as server8 had it: volumes and extents changing, one slab.
{
    echo "[slabs +0.19s] records read (0.12s)"
    for v in cilium busybox fedora.golden; do
        e=0
        while [ $e -lt 400 ]; do
            echo "2026-10-21T17:41:39.7381${e}Z  WARN stormblock::volume: Volume '$v' extent $e: slab e77ec45a-a733-466b-8540-34356f547e49 not attached, mapping dropped"
            e=$((e + 1))
        done
    done
    echo "Install: 12.31 → 12.37 on /dev/sda: 3 volume(s) in the data half, kept"
    echo "Flow-over: /dev/sda is laid out and handed to the engine that adopts this boot"
} > "$WORK/log"

engine_follow_filter < "$WORK/log" > "$WORK/out"
check "the flood is three lines and a count" 7 "$(wc -l < "$WORK/out" | tr -d ' ')"
check "the first line kept" "[slabs +0.19s] records read (0.12s)" "$(sed -n 1p "$WORK/out")"
check "the count names how many and where" \
    "  ... and 1197 more line(s) like that (all of them in /run/stormblock/engine.log)" \
    "$(grep 'more line' "$WORK/out")"
check "what follows the flood is shown" "Flow-over: /dev/sda is laid out and handed to the engine that adopts this boot" \
    "$(tail -n 1 "$WORK/out")"
check "the next stage line shown" 1 "$(grep -c '^Install: 12.31' "$WORK/out")"

# Lines that differ in their words are all shown.
printf '%s\n' "Attached system slab /dev/sda (array c777)" "Attached data slab /dev/sda (array fd5c)" \
    "Attached data slab /dev/sdb (array 0961)" "Boot volume: stormpump (92f1)" > "$WORK/log2"
engine_follow_filter < "$WORK/log2" > "$WORK/out2"
check "different lines all shown" 4 "$(wc -l < "$WORK/out2" | tr -d ' ')"

# Three of a kind and no more: nothing counted.
printf '%s\n' "a 1" "a 2" "a 3" "b" > "$WORK/log3"
engine_follow_filter < "$WORK/log3" > "$WORK/out3"
check "a run of three is shown whole" "a 1 a 2 a 3 b" "$(tr '\n' ' ' < "$WORK/out3" | sed 's/ $//')"

# A run at the end of the input is counted at the end.
printf '%s\n' "x 1" "x 2" "x 3" "x 4" "x 5" > "$WORK/log4"
engine_follow_filter < "$WORK/log4" > "$WORK/out4"
check "a run at the end is counted" 1 "$(grep -c 'and 2 more line' "$WORK/out4")"

# As it runs: lines written to a growing log reach the output before the
# input ends (nothing waits for the end of a run but the count).
: > "$WORK/live"
( tail -n +1 -f "$WORK/live" 2>/dev/null | engine_follow_filter > "$WORK/out5" ) &
P=$!
echo "Root device ready: /dev/ublkb0" >> "$WORK/live"
n=0
while [ $n -lt 50 ] && ! grep -q 'Root device ready' "$WORK/out5" 2>/dev/null; do sleep 0.1; n=$((n + 1)); done
check "a line is shown as it comes" 1 "$(grep -c 'Root device ready' "$WORK/out5" 2>/dev/null || echo 0)"
kill $P 2>/dev/null || :
pkill -f "tail -n +1 -f $WORK/live" 2>/dev/null || :

[ $fail -eq 0 ] && echo "PASS: engine follower" || { echo "FAIL: engine follower"; exit 1; }
