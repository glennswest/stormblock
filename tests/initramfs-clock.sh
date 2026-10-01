#!/bin/sh
# The initramfs clock step (#251): one bounded NTP try to the lease's servers,
# then fixed addresses; the RTC written on a step; the build date as a floor
# when nothing answered. The X9 blades have no RTC battery, so without this a
# node boots at 2000 and checks certificates against it.
#
# Runs the real code: the block is extracted from the init script this repo
# generates, between its marker comments, so the test cannot drift. date,
# ntpd, hwclock and timeout are stubs over a fake clock.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN clock step/,/# --- END clock step/p' "$GEN" > "$WORK/clock.sh"
[ -s "$WORK/clock.sh" ] || { echo "FAIL: could not extract the clock step block"; exit 1; }

REAL_DATE=$(command -v date)
BIN="$WORK/bin"; mkdir -p "$BIN"

# date over $CLK: +%s and other formats read it, -s @N sets it, -d @N formats N
cat > "$BIN/date" <<EOF
#!/bin/sh
set -- "\$@"
when=""; fmt=""; set_to=""
while [ \$# -gt 0 ]; do
    case "\$1" in
        -u) ;;
        -s) shift; set_to="\$1" ;;
        -d) shift; when="\$1" ;;
        +*) fmt="\$1" ;;
    esac
    shift
done
if [ -n "\$set_to" ]; then
    [ -n "\${STUB_DATE_SET_FAIL:-}" ] && exit 1
    echo "\${set_to#@}" > "\$CLK"; exit 0
fi
[ -n "\$when" ] || when="@\$(cat "\$CLK")"
exec $REAL_DATE -u -d "\$when" "\${fmt:-+%s}"
EOF
# ntpd: every -p server in STUB_NTP_OK answers and sets $CLK to STUB_NTP_TIME
cat > "$BIN/ntpd" <<'EOF'
#!/bin/sh
echo "$*" >> "$STUB_LOG.ntpd"
while [ $# -gt 0 ]; do
    if [ "$1" = -p ]; then
        shift
        case " ${STUB_NTP_OK:-} " in *" $1 "*)
            echo "$STUB_NTP_TIME" > "$CLK"
            [ -n "${STUB_NTP_QUIET:-}" ] || echo "ntpd: reply from $1: offset:+1.0 delay:0.01 status:0x24" >&2
            exit 0 ;;
        esac
    fi
    shift
done
exit 1
EOF
cat > "$BIN/timeout" <<'EOF'
#!/bin/sh
echo "$1" >> "$STUB_LOG.timeout"; shift; exec "$@"
EOF
cat > "$BIN/hwclock" <<'EOF'
#!/bin/sh
echo "$*" >> "$STUB_LOG.hwclock"
[ -z "${STUB_NO_RTC:-}" ]
EOF
chmod +x "$BIN"/*

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}
has() { # name needle haystack
    case "$3" in *"$2"*) echo "  ok    $1" ;; *) echo "  FAIL  $1: no '$2' in: $3"; fail=1 ;; esac
}
hasnt() {
    case "$3" in *"$2"*) echo "  FAIL  $1: unexpected '$2' in: $3"; fail=1 ;; *) echo "  ok    $1" ;; esac
}

Y2000=946684800       # what an X9 blade's RTC says after a power cut
BUILT=1790000000      # the image's build date
NOW=1790500000        # what the time servers say

# run <name> <clock> <address> [ntp-servers] -> output in $OUT, clock in $CLOCK
run() {
    n=$1; echo "$2" > "$WORK/clk.$n"
    rm -f "$WORK/log.$n".*
    if [ -n "${4:-}" ]; then echo "$4" > "$WORK/ntp.$n"; else rm -f "$WORK/ntp.$n"; fi
    echo "100.42 50.0" > "$WORK/uptime"
    echo "$BUILT" > "$WORK/built"
    OUT=$(
        PATH="$BIN:$PATH" CLK="$WORK/clk.$n" STUB_LOG="$WORK/log.$n" \
        STUB_NTP_TIME="$NOW" STUB_NTP_OK="$STUB_NTP_OK" \
        STUB_NTP_QUIET="$STUB_NTP_QUIET" STUB_NO_RTC="$STUB_NO_RTC" \
        STUB_DATE_SET_FAIL="$STUB_DATE_SET_FAIL" NTP_MODE="$NTP_MODE" \
        STORM_NTP_WAIT="$STORM_NTP_WAIT" \
        STORM_UPTIME="$WORK/uptime" STORM_NTP_SERVERS="$WORK/ntp.$n" \
        STORM_BUILD_DATE="$WORK/built" \
        sh -c '. "$1"; clock_step "$2"' sh "$WORK/clock.sh" "$3"
    )
    CLOCK=$(cat "$WORK/clk.$n")
    NTPD=$(cat "$WORK/log.$n.ntpd" 2>/dev/null | tr '\n' '|')
    HW=$(cat "$WORK/log.$n.hwclock" 2>/dev/null || true)
    WAIT=$(cat "$WORK/log.$n.timeout" 2>/dev/null | tr '\n' ' ')
    reset
}
# The knobs a case sets, back to their defaults after every run.
reset() {
    STUB_NTP_OK=""; STUB_NTP_QUIET=""; STUB_NO_RTC=""; STUB_DATE_SET_FAIL=""
    NTP_MODE=""; STORM_NTP_WAIT=3
}
reset

echo "the lease's server answers"
STUB_NTP_OK=10.0.0.5; run dhcp $Y2000 192.168.11.30/24 10.0.0.5
check "clock set" $NOW "$CLOCK"
has "says how far and from whom" "clock stepped by +$((NOW - Y2000)) s from 10.0.0.5" "$OUT"
check "one ntpd try, to the lease's server" "-n -q -d -p 10.0.0.5|" "$NTPD"
check "bounded at 3 s" "3 " "$WAIT"
check "RTC written as UTC" "-w -u" "$HW"
has "RTC said" "RTC written" "$OUT"

echo "the lease's servers are silent, a fixed address answers"
STUB_NTP_OK=216.239.35.0; run fallback $Y2000 192.168.11.30/24 "10.0.0.5 10.0.0.6"
check "clock set" $NOW "$CLOCK"
has "from the fixed address" "from 216.239.35.0" "$OUT"
check "lease first, then fixed addresses (no names)" \
    "-n -q -d -p 10.0.0.5 -p 10.0.0.6|-n -q -d -p 162.159.200.1 -p 216.239.35.0|" "$NTPD"
check "each try bounded" "3 3 " "$WAIT"

echo "no option 42"
STUB_NTP_OK=162.159.200.1; run nolease $Y2000 10.1.1.1/16
check "only the fixed addresses asked" "-n -q -d -p 162.159.200.1 -p 216.239.35.0|" "$NTPD"
check "clock set" $NOW "$CLOCK"

echo "no time server answers, clock before the build date"
STUB_NTP_OK=; run floor $Y2000 192.168.11.30/24 10.0.0.5
check "clock floored at the build date" $BUILT "$CLOCK"
has "said loudly" "BEFORE THIS IMAGE WAS BUILT" "$OUT"
has "says what it was" "2000-01-01 00:00:00" "$OUT"
has "says no server answered" "no time server answered" "$OUT"
check "RTC not written with a guess" "" "$HW"

echo "no time server answers, clock already after the build date"
STUB_NTP_OK=; run late $((BUILT + 5000)) 192.168.11.30/24
check "clock left alone" $((BUILT + 5000)) "$CLOCK"
hasnt "no floor message" "BEFORE THIS IMAGE" "$OUT"

echo "a clock ahead is stepped back"
STUB_NTP_OK=10.0.0.5; run ahead $((NOW + 3600)) 192.168.11.30/24 10.0.0.5
has "negative step" "clock stepped by -3600 s from 10.0.0.5" "$OUT"

echo "no network: nothing asked, floor applies"
STUB_NTP_OK=10.0.0.5; run nonet $Y2000 "" 10.0.0.5
check "ntpd not run" "" "$NTPD"
has "says why" "not stepped (no network)" "$OUT"
check "floored" $BUILT "$CLOCK"

echo "link-local only: nothing asked"
STUB_NTP_OK=10.0.0.5; run ll $((BUILT + 1)) 169.254.1.1/16 10.0.0.5
check "ntpd not run" "" "$NTPD"
has "says why" "link-local" "$OUT"

echo "rd.stormblock.ntp=off"
NTP_MODE=off; STUB_NTP_OK=10.0.0.5; run off $Y2000 192.168.11.30/24 10.0.0.5
check "ntpd not run" "" "$NTPD"
has "says so" "rd.stormblock.ntp=off" "$OUT"
check "floor still applies" $BUILT "$CLOCK"

echo "no RTC on the machine"
STUB_NO_RTC=1; STUB_NTP_OK=10.0.0.5; run nortc $Y2000 192.168.11.30/24 10.0.0.5
check "clock still set" $NOW "$CLOCK"
has "says the RTC was not written" "RTC not written" "$OUT"

echo "ntpd names no peer"
STUB_NTP_QUIET=1; STUB_NTP_OK=10.0.0.6; run quiet $Y2000 192.168.11.30/24 "10.0.0.5 10.0.0.6"
has "names the servers it asked" "from 10.0.0.5,10.0.0.6" "$OUT"

echo "STORM_NTP_WAIT bounds each try"
STORM_NTP_WAIT=1; STUB_NTP_OK=; run wait $((BUILT + 1)) 192.168.11.30/24 10.0.0.5
check "the wait passed to timeout" "1 1 " "$WAIT"

echo "a clock that cannot be set"
STUB_DATE_SET_FAIL=1; STUB_NTP_OK=; run noset $Y2000 192.168.11.30/24
has "says it could not" "could not be set" "$OUT"

echo "the whole /init still parses"
sed -n "/^cat > \"\$INITRD_DIR\/init\" << 'INITSCRIPT'/,/^INITSCRIPT/p" "$GEN" | sed '1d;$d' > "$WORK/init"
if sh -n "$WORK/init"; then echo "  ok    sh -n /init"; else echo "  FAIL  sh -n /init"; fail=1; fi

[ "$fail" = 0 ] && echo "PASS" || { echo "FAILED"; exit 1; }
