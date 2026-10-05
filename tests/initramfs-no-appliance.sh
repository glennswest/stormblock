#!/bin/sh
# What /init does when the appliance does not answer (#294).
#
# server8, installing 11.82 over a disk that held an older release: forge did
# not answer the one 3-second health check made right after the ConnectX-3
# link came up, so BOOTHOST stayed empty, the local-slab probe and the release
# check were skipped without a word, the old disk went to `boot-local`, and
# the engine died on `volume 'kubelet-data' not found` - an error that
# scrolled off the screen above `FATAL: root device /dev/ublkb0 not found`.
#
# Pinned here:
#   * a boothost the network names is asked again for a while before /init
#     goes on without it, and why there is none is kept (BOOTHOST_WHY);
#   * with no appliance the probe still runs: a disk missing a volume the
#     mount list names stops the boot with a message naming it, and a disk
#     that can boot boots with "RELEASE CHECK SKIPPED" said, not silently;
#   * the release a disk holds is read from its root volume's os-release;
#   * the engine's last lines are repeated after the FATAL.
#
# Runs the real code: the blocks are extracted from the init script this repo
# generates, between their marker comments, so the test cannot drift.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

extract() { # name file
    sed -n "/# --- BEGIN $1/,/# --- END $1/p" "$GEN" > "$WORK/$2"
    [ -s "$WORK/$2" ] || { echo "FAIL: could not extract the $1 block"; exit 1; }
}
extract "appliance discovery" discovery.sh
extract "local-slab probe" probe.sh
extract "mount list" mounts.sh
extract "engine report" report.sh

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}
contains() { # name needle haystack
    case "$3" in
    *"$2"*) echo "  ok    $1" ;;
    *) echo "  FAIL  $1: '$2' not in: $3"; fail=1 ;;
    esac
}

# ---------------------------------------------------------------------------
echo "appliance discovery:"

# wget answers from a counter: the first $ANSWER_AFTER asks fail.
discover() { # named-host -> "BOOTHOST|BOOTHOST_WHY|asks"
    (
        set +e
        BOOTHOST=""
        printf '%s' "$1" > "$WORK/named"
        STORM_BOOTHOST_FILE="$WORK/named"
        STORM_BOOTHOST_RETRY=0
        echo 0 > "$WORK/asks"
        wget() {
            n=$(($(cat "$WORK/asks") + 1)); echo "$n" > "$WORK/asks"
            case "$*" in
            *"${ANSWERS:-nothing-answers}/api/v1/health"*) [ "$n" -gt "${ANSWER_AFTER:-0}" ] ;;
            *) return 1 ;;
            esac
        }
        nslookup() { return 1; }
        . "$WORK/discovery.sh" > "$WORK/discovery.out" 2>&1
        echo "$BOOTHOST|$BOOTHOST_WHY|$(cat "$WORK/asks")"
    )
}

r=$(ANSWERS=http://forge:9090 ANSWER_AFTER=0 STORM_BOOTHOST_WAIT=5 discover http://forge:9090)
check "a named boothost that answers is the appliance" "http://forge:9090" "${r%%|*}"

# server8: the link has just come up, the first asks go nowhere.
r=$(ANSWERS=http://forge:9090 ANSWER_AFTER=12 STORM_BOOTHOST_WAIT=30 discover http://forge:9090)
check "a named boothost is asked again until it answers" "http://forge:9090" "${r%%|*}"
contains "and the console says it took tries" "answered after" "$(cat "$WORK/discovery.out")"

r=$(STORM_BOOTHOST_WAIT=1 discover http://forge:9090)
check "a named boothost that never answers: none" "" "${r%%|*}"
why=$(printf '%s' "$r" | cut -d'|' -f2)
contains "and why is kept, naming it" "the boothost the network names (http://forge:9090) did not answer" "$why"
asks=${r##*|}
[ "$asks" -gt 4 ] && echo "  ok    and it was asked more than once ($asks asks)" \
    || { echo "  FAIL  it was asked $asks time(s), not again"; fail=1; }

r=$(STORM_BOOTHOST_WAIT=30 discover "")
check "with no named boothost nothing is waited for" "" "${r%%|*}"
contains "and why is kept" "no appliance answered" "$(printf '%s' "$r" | cut -d'|' -f2)"
start=$(date +%s); discover "" > /dev/null; took=$(( $(date +%s) - start ))
[ "$took" -lt 5 ] && echo "  ok    and it does not wait (${took}s)" \
    || { echo "  FAIL  it waited ${took}s with no named boothost"; fail=1; }

# ---------------------------------------------------------------------------
echo "local-slab probe without an appliance:"

# A stormblock whose `slab list` answers from the device's file's first line
# and `slab volumes` from the rest; `slab cat` writes an os-release.
STUB="$WORK/stormblock"
cat > "$STUB" <<'STUBEOF'
#!/bin/sh
if [ "$1 $2" = "slab cat" ]; then
    out=""
    while [ $# -gt 0 ]; do
        [ "$1" = "--out" ] && { out="$2"; shift; }
        shift
    done
    [ -n "${STUB_OS_RELEASE:-}" ] || exit 1
    printf '%s\n' "$STUB_OS_RELEASE" > "$out"
    exit 0
fi
answers="$STUB_ANSWERS/$(basename "$3")"
case "$2" in
list)    sed -n '1p' "$answers" 2>/dev/null ;;
volumes) sed -n '2,$p' "$answers" 2>/dev/null ;;
esac
exit 0
STUBEOF
chmod +x "$STUB"
mkdir -p "$WORK/answers"
answer_for() { f="$WORK/answers/$1"; shift; printf '%s\n' "$@" > "$f"; }

probe() { # slab mounts -> "SLAB|PROBE_STOP" ; console output in $WORK/probe.out
    (
        set +e
        STORM_STORMBLOCK="$STUB"; STUB_ANSWERS="$WORK/answers"; STORM_RUN="$WORK"
        export STORM_STORMBLOCK STUB_ANSWERS STORM_RUN STUB_OS_RELEASE
        HOOK_DECIDED=""; SLAB="$1"; VOLUME=""; META=""; ASSIMILATE=any
        BOOTHOST=""; BOOTHOST_WHY="the boothost the network names (http://forge:9090) did not answer in 90s (31 tries)"
        mounts_from() { MOUNTS="$TEST_MOUNTS"; MOUNTS_FROM="the command line"; }
        TEST_MOUNTS="$2"
        . "$WORK/probe.sh" > "$WORK/probe.out" 2>&1
        echo "$SLAB|$PROBE_STOP"
    )
}

old="$WORK/sda"; : > "$old"
answer_for sda "$old: slab 11111111-2222-3333-4444-555555555555 (role=system, tier=hot)" \
    "$old: volume stormpump 4096 MiB 9f1c0000-0000-0000-0000-000000000001" \
    "$old: volume pod-logs 256 MiB 9f1c0000-0000-0000-0000-000000000002"

# server8: the new release mounts kubelet-data, the old disk has none.
r=$(probe "$old" "kubelet-data:/var/lib/kubelet,pod-logs:/var/log/pods")
contains "a disk missing a mounted volume stops the boot" "missing 1 mounted volume(s)" "${r#*|}"
contains "and the console names the volume" "kubelet-data" "$(cat "$WORK/probe.out")"
check "and the slab is not handed to boot-local as if it were fine" "$old" "${r%%|*}"

r=$(probe "$old" "pod-logs:/var/log/pods")
check "a disk with everything boots" "$old|" "$r"
contains "and the skipped release check is said" "RELEASE CHECK SKIPPED: the boothost the network names" \
    "$(cat "$WORK/probe.out")"

notslab="$WORK/sdb"; : > "$notslab"
answer_for sdb "$notslab: not a slab"
r=$(probe "$notslab" "")
contains "a device that is not a slab stops the boot" "is not a slab" "${r#*|}"

# With an appliance, as before: ask it.
r=$( (
    set +e
    STORM_STORMBLOCK="$STUB"; STUB_ANSWERS="$WORK/answers"; export STORM_STORMBLOCK STUB_ANSWERS
    HOOK_DECIDED=""; SLAB="$old"; VOLUME=""; META=""; ASSIMILATE=off
    BOOTHOST="http://forge:9090"
    mounts_from() { MOUNTS="kubelet-data:/var/lib/kubelet"; }
    . "$WORK/probe.sh" > /dev/null 2>&1
    echo "$SLAB|$PROBE_STOP"
) )
check "with an appliance a short disk still goes to it" "|" "$r"

# ---------------------------------------------------------------------------
echo "the release a disk holds:"
release() ( set +e; MOUNTS=""; VOLUME=""; STORM_STORMBLOCK="$STUB"; STORM_RUN="$WORK"; STUB_OS_RELEASE="$1"; export STUB_OS_RELEASE
            . "$WORK/mounts.sh" >/dev/null 2>&1; disk_release "$old" )
check "PRETTY_NAME" "stormcos 11.79" "$(release 'NAME=stormcos
PRETTY_NAME="stormcos 11.79"
VERSION_ID=11.79')"
check "VERSION_ID when there is no PRETTY_NAME" "11.79" "$(release 'VERSION_ID=11.79')"
contains "no os-release says so" "unknown" "$(release '')"

# ---------------------------------------------------------------------------
echo "engine report:"
report() ( ENGINE_LOG="$1"; . "$WORK/report.sh"; engine_report 25 )
i=1; : > "$WORK/engine.log"
while [ $i -le 40 ]; do echo "line $i" >> "$WORK/engine.log"; i=$((i + 1)); done
echo "Error: volume 'kubelet-data' not found in slab metadata" >> "$WORK/engine.log"
out=$(report "$WORK/engine.log")
contains "the engine's last error is repeated" "| Error: volume 'kubelet-data' not found" "$out"
check "with its last 25 lines" 26 "$(printf '%s\n' "$out" | wc -l | tr -d ' ')"
: > "$WORK/empty.log"
contains "an engine that wrote nothing is said" "(it wrote nothing)" "$(report "$WORK/empty.log")"

[ "$fail" = 0 ] && echo "all no-appliance cases pass" || { echo "FAILURES"; exit 1; }
