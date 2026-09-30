#!/bin/sh
# The initramfs console fan-out: every console= hears the boot (#237).
#
# /dev/console is only the last console= on the kernel line, so with
# `console=tty0 console=ttyS0,115200n8` a server's screen saw the kernel and
# then nothing. /init now writes to each console that is there. This pins
# which consoles it picks, that each gets every line, that a killed tee does
# not break the pipe for the writers (the engine's println! panics on EPIPE),
# and that one console changes nothing.
#
# Runs the real code: the block is extracted from the init script this repo
# generates, between its two marker comments. Consoles are plain files in a
# scratch directory standing in for /dev.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
mkdir -p "$HERE/../tmp"
WORK=$(mktemp -d "$HERE/../tmp/console.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN console fan-out/,/# --- END console fan-out/p' "$GEN" > "$WORK/console.sh"
[ -s "$WORK/console.sh" ] || { echo "FAIL: could not extract the console fan-out"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

DEV="$WORK/dev"
mkdir -p "$DEV"

# Which consoles: the list only, cmdline order, options dropped, a device
# that is not there skipped, a repeat once.
pick() { # cmdline -> the picked devices, space-separated, relative to $DEV
    printf '%s\n' "$1" > "$WORK/cmdline"
    (
        STORM_CONSOLE_DEV="$DEV" STORM_CMDLINE="$WORK/cmdline"
        # Not this host's /sys: its ttyS0 may have no UART.
        STORM_CONSOLE_SYS="${STORM_CONSOLE_SYS:-$WORK/nosys}"
        STORM_CONSOLE_FIFO="$WORK/pick.fifo"
        # Only the functions: the block's last lines would start a fan-out.
        eval "$(sed '/^CONSOLE_FANOUT_PID=""$/,$d' "$WORK/console.sh")"
        console_list "$(cat "$WORK/cmdline")" | sed "s|^$DEV/||" | tr '\n' ' ' | sed 's/ $//'
    )
}

: > "$DEV/tty0"; : > "$DEV/ttyS0"; : > "$DEV/hvc0"
check "tty0 and ttyS0, options dropped" "tty0 ttyS0" \
    "$(pick 'BOOT_IMAGE=/vmlinuz root=/dev/ublkb0 console=tty0 console=ttyS0,115200n8 quiet')"
check "cmdline order kept" "ttyS0 tty0" "$(pick 'console=ttyS0,115200n8 console=tty0')"
check "a console that is not there is skipped" "tty0" "$(pick 'console=tty0 console=ttyS1,115200')"
check "a repeat counts once" "tty0 ttyS0" "$(pick 'console=tty0 console=ttyS0 console=/dev/tty0')"
check "no console= picks nothing" "" "$(pick 'root=/dev/ublkb0 quiet')"
check "not a device name" "" "$(pick 'console=uart8250,io,0x3f8 console= console=null')"
chmod 0444 "$DEV/hvc0"
if ! ( : >> "$DEV/hvc0" ) 2>/dev/null; then
    check "a console that will not open is skipped" "tty0" "$(pick 'console=hvc0 console=tty0')"
fi
chmod 0644 "$DEV/hvc0"
mkdir -p "$WORK/sys/ttyS1" "$WORK/sys/ttyS0"; : > "$DEV/ttyS1"
echo 0 > "$WORK/sys/ttyS1/type"; echo 4 > "$WORK/sys/ttyS0/type"
check "a serial port with no UART (type 0) is skipped" "tty0 ttyS0" \
    "$(STORM_CONSOLE_SYS="$WORK/sys" pick 'console=tty0 console=ttyS1 console=ttyS0,115200n8')"

# Every line to every console, from /init and from what it starts.
fanout() { # cmdline — runs a stand-in /init, prints nothing itself
    printf '%s\n' "$1" > "$WORK/cmdline"
    : > "$DEV/tty0"; : > "$DEV/ttyS0"
    (
        STORM_CONSOLE_DEV="$DEV" STORM_CMDLINE="$WORK/cmdline"
        # Not this host's /sys: its ttyS0 may have no UART.
        STORM_CONSOLE_SYS="${STORM_CONSOLE_SYS:-$WORK/nosys}"
        STORM_CONSOLE_FIFO="$WORK/fan.fifo"
        . "$WORK/console.sh"
        echo "INSTALL: a release the disk does not hold"
        echo "FATAL: to stderr" >&2
        # A child that outlives the script, as the engine does switch_root.
        ( sleep 1; echo "Flow-over: from the engine" ) &
        if [ -n "$CONSOLE_FANOUT_PID" ]; then
            # Kill the tee: the reader must run it again, and a writer must
            # never see a broken pipe.
            sleep 0.3
            k=0
            for t in $(pgrep -P "$CONSOLE_FANOUT_PID" tee 2>/dev/null); do
                kill -9 "$t" && k=$((k + 1))
            done
            echo "$k" > "$WORK/killed"
            sleep 0.2
            echo "after the tee was killed"
        fi
        exec > /dev/null 2>&1
        wait
    ) > "$WORK/stdout" 2>&1
    # The fan-out ends when its last writer closes.
    i=0
    while [ "$i" -lt 50 ] && pgrep -f "tee -a $DEV/" >/dev/null 2>&1; do
        sleep 0.1; i=$((i + 1))
    done
}

fanout 'console=tty0 console=ttyS0,115200n8'
for c in tty0 ttyS0; do
    check "$c: /init's stdout" 1 "$(grep -c '^INSTALL: a release' "$DEV/$c" || true)"
    check "$c: /init's stderr" 1 "$(grep -c '^FATAL: to stderr' "$DEV/$c" || true)"
    check "$c: a child after /init is gone" 1 "$(grep -c '^Flow-over: from the engine' "$DEV/$c" || true)"
    check "$c: a line after tee was killed" 1 "$(grep -c '^after the tee was killed' "$DEV/$c" || true)"
done
check "a tee was killed" 1 "$(cat "$WORK/killed")"
check "nothing left on the old stdout" "" "$(cat "$WORK/stdout")"
check "the fan-out ended with its writers" "" "$(pgrep -f "tee -a $DEV/" || true)"

# One console: nothing changes — stdout stays where it was.
fanout 'console=ttyS0,115200n8'
check "one console: no fan-out, stdout untouched" 1 "$(grep -c '^INSTALL: a release' "$WORK/stdout" || true)"
check "one console: the device is not written" "" "$(cat "$DEV/ttyS0")"

if [ "$fail" -ne 0 ]; then
    echo "initramfs console fan-out: FAILED"
    exit 1
fi
echo "initramfs console fan-out: all ok"
