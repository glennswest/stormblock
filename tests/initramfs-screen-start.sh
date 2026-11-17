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

[ "$fail" = 0 ] && echo "initramfs-screen-start: all ok" || { echo "initramfs-screen-start: FAILED"; exit 1; }
