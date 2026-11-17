#!/bin/sh
# /init's start of the screen (#618) and the power button (#635): each
# program's own line reaches the console, not the binary's start-up line
# (pvetest3 12.91 echoed `StormBlock starting, config: …` for the screen, and
# `screen: drawing on …` never reached the console).
#
# Runs the real code: the block is extracted from the init script this repo
# generates, between its marker comments.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
W=$(mktemp -d)
trap 'kill $(cat "$W"/pids 2>/dev/null) 2>/dev/null; rm -rf "$W"' EXIT
sed -n '/# --- BEGIN screen start/,/# --- END screen start/p' "$GEN" > "$W/block.sh"
grep -q 'END screen start' "$W/block.sh" || { echo "FAIL: could not extract the screen start block"; exit 1; }

fail=0
has() { if grep -q -- "$2" "$W/out"; then echo "  ok    $1"; else echo "  FAIL  $1: no '$2'"; sed 's/^/  | /' "$W/out"; fail=1; fi; }
hasnt() { if grep -q -- "$2" "$W/out"; then echo "  FAIL  $1: '$2'"; sed 's/^/  | /' "$W/out"; fail=1; else echo "  ok    $1"; fi; }

# The binary: its start-up tracing line at once, its own line 0.5 s later,
# then it stays up (as the screen and the watcher do).
cat > "$W/sb" <<'SB'
#!/bin/sh
echo "2026-11-17T02:36:24.931227Z  INFO stormblock::cli: StormBlock starting, config: /etc/stormblock/stormblock.toml"
sleep 0.5
case "$1" in
screen) echo "screen: drawing on /dev/dri/card0 connector 39 crtc 37 1280x800 (1280x800) (#618)" ;;
power-button) echo "power button: watching /dev/input/event0 (Power Button) (a press powers off cleanly) (#635)" ;;
esac
exec sleep 30
SB
chmod +x "$W/sb"

run() {
    {
        echo "STORM_RUN=$W/run STORM_SCREEN_BIN=$W/sb STORM_POWERBUTTON_BIN=$W/sb STORM_INSTALL_TICKET=$W/none"
        cat "$W/block.sh"
        echo 'power_button_start; screen_start; echo "$POWERBUTTON_PID $SCREEN_PID" > "'"$W"'/pids"'
    } > "$W/run.sh"
    mkdir -p "$W/run"
    sh "$W/run.sh" > "$W/out" 2>&1
}

echo "screen start: each program's own line on the console"
run
has "the screen's line" "^  screen: drawing on /dev/dri/card0"
has "the power button's line" "^  power button: watching /dev/input/event0 (Power Button)"
hasnt "never the start-up line" "StormBlock starting"

# The hand-over (#638): the screen is told (SIGUSR1) and stays up when it
# answers (screen.follow); one that does not answer is stopped.
cat > "$W/follow" <<'SB'
#!/bin/sh
echo "screen: drawing on /dev/dri/card0 (#618)"
[ "${NO_ANSWER:-}" = 1 ] && trap '' USR1 || trap 'echo armed > "$RUN/screen.follow"' USR1
while :; do sleep 0.1; done
SB
chmod +x "$W/follow"
handover() { # env -> console in out, the screen's pid in pid
    {
        echo "STORM_RUN=$W/run STORM_SCREEN_BIN=$W/follow STORM_INSTALL_TICKET=$W/none RUN=$W/run export RUN $1"
        cat "$W/block.sh"
        echo 'screen_start; P=$SCREEN_PID; screen_switch_root; echo "$P" > "'"$W"'/pid"; echo "=AFTER=$SCREEN_PID"'
    } > "$W/ho.sh"
    rm -rf "$W/run"; mkdir -p "$W/run"
    sh "$W/ho.sh" > "$W/out" 2>&1
}
echo "screen start: the hand-over"
handover "NO_ANSWER=0 export NO_ANSWER"
has "it answers: it stays up into stormpump's steps" "the screen stays up through the hand-over, into stormpump's steps until Ready"
if kill -0 "$(cat "$W/pid")" 2>/dev/null; then echo "  ok    and runs on"; kill "$(cat "$W/pid")"; else echo "  FAIL  it was stopped"; fail=1; fi
handover "NO_ANSWER=1 export NO_ANSWER"
has "it does not answer: stopped, the display given back" "the screen did not answer the hand-over: stopped"
has "  and forgotten" "^=AFTER=$"
sleep 0.3
if kill -0 "$(cat "$W/pid")" 2>/dev/null; then echo "  FAIL  it still runs"; fail=1; kill -9 "$(cat "$W/pid")"; else echo "  ok    and is gone"; fi

[ "$fail" = 0 ] && echo "initramfs-screen-start: all ok" || { echo "initramfs-screen-start: FAILED"; exit 1; }
