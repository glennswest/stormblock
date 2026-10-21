#!/bin/sh
# The install path's log over the network (#477): /init keeps its own lines
# in init.log (through the console fan-out), starts `stormblock logcast` on
# init.log and the engine's log from its first lines, follows the engine's
# log onto the consoles alone (so the network has the engine's lines once,
# whole, not also the console's collapsed copy), and stops logcast before
# switch_root.
#
# Runs the real blocks, extracted from the init script this repo generates;
# logcast is a stand-in that records how it was started and stopped (the
# real one is tested in `logcast::tests` and ci-logcast-verify.sh).
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
mkdir -p "$HERE/../tmp"
WORK=$(mktemp -d "$HERE/../tmp/logcast.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

for b in "console fan-out" "logcast" "engine follower"; do
    sed -n "/# --- BEGIN $b/,/# --- END $b/p" "$GEN" >> "$WORK/blocks.sh"
done
grep -q 'logcast_start' "$WORK/blocks.sh" || { echo "FAIL: could not extract the blocks"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

DEV="$WORK/dev"; RUN="$WORK/run"
mkdir -p "$DEV" "$RUN"
: > "$DEV/ttyS0"
printf '%s\n' 'console=ttyS0,115200n8' > "$WORK/cmdline"

# The stand-in: says how it was started, and that it was stopped.
cat > "$WORK/stormblock" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" > "$LC_ARGS"
trap 'echo stopped > "$LC_STOPPED"; exit 0' TERM
while :; do sleep 0.05; done
EOF
chmod +x "$WORK/stormblock"

(
    STORM_CONSOLE_DEV="$DEV" STORM_CMDLINE="$WORK/cmdline" STORM_CONSOLE_SYS="$WORK/nosys"
    STORM_CONSOLE_FIFO="$RUN/console.fifo" STORM_ENGINE_FIFO="$RUN/engine-console.fifo"
    STORM_INIT_LOG="$RUN/init.log" STORM_ENGINE_LOG="$RUN/engine.log" ENGINE_LOG="$RUN/engine.log"
    STORM_LOGCAST_BIN="$WORK/stormblock"
    export LC_ARGS="$WORK/args" LC_STOPPED="$WORK/stopped"
    . "$WORK/blocks.sh"
    echo "INSTALL: a release the disk does not hold"
    i=0; while [ ! -s "$WORK/args" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
    # The engine writes its log; the follower shows it on the console.
    printf '%s\n' "Flow-over: 3 extents moved" "slab 1 not attached" > "$ENGINE_LOG"
    engine_follow_start
    sleep 1
    echo "before switch_root"
    logcast_stop
    echo "$LOGCAST_PID" > "$WORK/pid-after"
    kill "$FOLLOW_PID" 2>/dev/null || :
    exec > /dev/null 2>&1
) > "$WORK/stdout" 2>&1
i=0
while [ "$i" -lt 50 ] && pgrep -f "tee -a $DEV/" >/dev/null 2>&1; do sleep 0.1; i=$((i + 1)); done

pkill -f "tail -n +1 -f $RUN/engine.log" 2>/dev/null || true

check "logcast follows init.log" 1 "$(grep -c "^$RUN/init.log=stormblock-init\$" "$WORK/args" || true)"
check "logcast follows the engine's log" 1 "$(grep -c "^$RUN/engine.log=stormblock\$" "$WORK/args" || true)"
check "logcast's status beside them" 1 "$(grep -c "^$RUN/logcast.status\$" "$WORK/args" || true)"
check "logcast stopped before switch_root" stopped "$(cat "$WORK/stopped" 2>/dev/null)"
check "and forgotten" "" "$(cat "$WORK/pid-after")"
check "init.log has /init's lines" 2 "$(grep -c '^INSTALL: a release\|^before switch_root' "$RUN/init.log" || true)"
check "the console has /init's lines" 2 "$(grep -c '^INSTALL: a release\|^before switch_root' "$DEV/ttyS0" || true)"
check "the console has the engine's lines" 1 "$(grep -c '^Flow-over: 3 extents moved' "$DEV/ttyS0" || true)"
check "init.log does not have the engine's lines (engine.log has them)" 0 "$(grep -c '^Flow-over: 3 extents moved' "$RUN/init.log" || true)"
check "nothing on the old stdout" "" "$(cat "$WORK/stdout")"

# No stormblock (a test, an image without it): no logcast, nothing else changes.
rm -f "$WORK/args"
(
    STORM_CONSOLE_DEV="$DEV" STORM_CMDLINE="$WORK/cmdline" STORM_CONSOLE_SYS="$WORK/nosys"
    STORM_CONSOLE_FIFO="$RUN/console2.fifo" STORM_INIT_LOG="$RUN/init2.log"
    STORM_LOGCAST_BIN="$WORK/missing"
    . "$WORK/blocks.sh"
    echo "$LOGCAST_PID" > "$WORK/pid-none"
    logcast_stop
    exec > /dev/null 2>&1
) > "$WORK/stdout" 2>&1
check "no binary: no logcast" "" "$(cat "$WORK/pid-none")"

if [ "$fail" -ne 0 ]; then
    echo "initramfs logcast: FAILED"
    exit 1
fi
echo "initramfs logcast: all ok"
