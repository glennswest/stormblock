#!/bin/sh
# The storage layout's tools (#573): only stormraid and stormdrive, only a
# static ELF for the image's machine. The block of
# scripts/build-stormblock-initramfs.sh, against hand-made ELF headers.
set -u
GEN=scripts/build-stormblock-initramfs.sh
W=$(mktemp -d "${TMPDIR:-/tmp}/initramfs-tools.XXXXXX")
trap 'rm -rf "$W"' EXIT
FAILS=0
ok()   { echo "  ok    $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
says() { # what got pattern
    case "$2" in
        $3) ok "$1" ;;
        *) fail "$1: got '$2', want '$3'" ;;
    esac
}

sed -n '/^# --- BEGIN target architecture/,/^# --- END target architecture/p' "$GEN" > "$W/blocks"
sed -n '/^# --- BEGIN initramfs tools/,/^# --- END initramfs tools/p' "$GEN" >> "$W/blocks"
[ "$(grep -c '^# --- BEGIN' "$W/blocks")" = 2 ] || { echo "FAIL: the blocks are not in $GEN"; exit 1; }
. "$W/blocks"

elf() { # file machine-low machine-high (octal)
    { printf '\177ELF'; head -c 14 /dev/zero; printf "\\$2\\$3"; head -c 44 /dev/zero; } > "$1"
}
elf "$W/x86" 076 000
elf "$W/arm" 267 000
{ cat "$W/x86"; printf '/lib64/ld-linux-x86-64.so.2'; } > "$W/dyn"
printf '#!/bin/sh\nexit 0\n' > "$W/script"

says "a static x86_64 stormraid goes in" "$(tool_refused stormraid "$W/x86" 62)" ""
says "a static x86_64 stormdrive goes in" "$(tool_refused stormdrive "$W/x86" 62)" ""
says "an aarch64 tool in an x86_64 image" "$(tool_refused stormraid "$W/arm" 62)" "*machine 183*62*"
says "a dynamic tool" "$(tool_refused stormdrive "$W/dyn" 62)" "*dynamically linked*"
says "a script is no tool" "$(tool_refused stormraid "$W/script" 62)" "*not an ELF*"
says "a missing file" "$(tool_refused stormraid "$W/none" 62)" "*not found*"
says "another name" "$(tool_refused fio "$W/x86" 62)" "*not a tool this image carries*"
says "an aarch64 tool in an aarch64 image" "$(tool_refused stormraid "$W/arm" 183)" ""

[ "$FAILS" = 0 ] || { echo "$FAILS failure(s)"; exit 1; }
echo "initramfs-tools: all ok"
