#!/bin/sh
# The install-config.yaml stormbootx hands down (#275, stormbootx#79): read
# from volatile EFI variables, used only when its length and sha256 match the
# header, every variable deleted (they carry secrets), written to /state on a
# first boot only, and never printed.
#
# Runs the real code: the identity block (efi_value) and the install config
# blocks are extracted from the init script this repo generates, between
# their marker comments, so the test cannot drift.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

for b in "boot identity" "install config read" "install config write"; do
    f="$WORK/$(echo "$b" | tr ' ' '-').sh"
    sed -n "/# --- BEGIN $b/,/# --- END $b/p" "$GEN" > "$f"
    [ -s "$f" ] || { echo "FAIL: could not extract the $b block"; exit 1; }
done

GUID="ab361f54-0166-44a4-a088-1ac22e98ab76"
EV="$WORK/efivars"
fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

var() { # name file -> the efivarfs file: 4 attribute bytes, then the value
    printf '\007\000\000\000' > "$EV/$1-$GUID"
    cat "$2" >> "$EV/$1-$GUID"
}

# A config of 2053 bytes (what stormbootx's own test hands down), with a
# secret in it that must never reach the console.
CONF="$WORK/install-config.yaml"
{
    echo "apiVersion: v1"
    echo "pullSecret: SECRET-PULL-MARKER"
    echo "apiToken: SECRET-TOKEN-MARKER"
    i=0
    while [ "$(wc -c < "$WORK/pad" 2>/dev/null || echo 0)" -lt 1 ]; do :; break; done
} > "$CONF"
while [ "$(wc -c < "$CONF")" -lt 2053 ]; do printf 'x' >> "$CONF"; done
SHA=$(sha256sum "$CONF" | cut -d' ' -f1)

lay() { # [header] -> the variables for $CONF, chunked at 768
    rm -rf "$EV"; mkdir -p "$EV"
    n=0
    while :; do
        dd if="$CONF" of="$WORK/chunk" bs=768 skip="$n" count=1 2>/dev/null
        [ -s "$WORK/chunk" ] || break
        var "StormBootInstallConfig$n" "$WORK/chunk"
        n=$((n + 1))
    done
    printf '%s' "${1:-v1:$(wc -c < "$CONF" | tr -d ' '):$n:$SHA}" > "$WORK/hdr"
    var StormBootInstallConfig "$WORK/hdr"
}

boot() { # mounted(yes|no) -> runs read then write; console in $WORK/console
    rm -rf "$WORK/run"
    : > "$WORK/mounts"
    [ "$1" = yes ] && echo "/dev/ublkb9 $WORK/sysroot/state ext4 rw 0 0" > "$WORK/mounts"
    STORM_EFIVARS="$EV" STORM_RUN="$WORK/run" STORM_SYSROOT="$WORK/sysroot" \
    STORM_MOUNTS_FILE="$WORK/mounts" sh -c '
        . "$0/boot-identity.sh"
        . "$0/install-config-read.sh"
        echo "STAGED=${INSTALL_CONFIG_STAGED:-}"
        [ -n "${INSTALL_CONFIG_STAGED:-}" ] && cp "$INSTALL_CONFIG_STAGED" "$0/staged-copy"
        . "$0/install-config-write.sh"
    ' "$WORK" > "$WORK/console" 2>&1 || true
}
left() { ls "$EV" 2>/dev/null | grep -c '^StormBootInstallConfig' || true; }
staged() { grep '^STAGED=' "$WORK/console" | cut -d= -f2; }
said() { grep -q "$1" "$WORK/console" && echo yes || echo no; }

echo "reading:"
lay; rm -rf "$WORK/sysroot"; mkdir -p "$WORK/sysroot/state"; rm -f "$WORK/staged-copy"
boot yes
check "a header and three chunks: verified" yes "$(said '2053 bytes from the boot media.*(verified)')"
check "what was staged is the file, byte for byte" "$SHA" "$(sha256sum "$WORK/staged-copy" | cut -d' ' -f1)"
check "every variable is deleted after reading" 0 "$(left)"
check "the content is never printed" no "$(grep -c 'SECRET-' "$WORK/console" >/dev/null && echo yes || echo no)"

lay "v1:2053:3:$(echo "$SHA" | tr 'a-f0-9' '0-9a-f')"; boot yes
check "a digest that does not match: not used" "" "$(staged)"
check "  and the variables are deleted anyway" 0 "$(left)"
lay "v1:2000:3:$SHA"; boot yes
check "a length that does not match: not used" "" "$(staged)"
lay; rm -f "$EV/StormBootInstallConfig1-$GUID"; boot yes
check "a missing chunk: not used" "" "$(staged)"
check "  and the rest are deleted" 0 "$(left)"
lay "v2:2053:3:$SHA"; boot yes
check "a header that is not v1: ignored" yes "$(said 'not one this initramfs reads')"
check "  and deleted" 0 "$(left)"
lay "v1 2053 3 $SHA"; boot yes
check "a header efi_value refuses: ignored and deleted" 0 "$(left)"
rm -rf "$EV"; mkdir -p "$EV"; boot yes
check "no header: nothing to do" "" "$(staged)"

echo "writing:"
lay; rm -rf "$WORK/sysroot"; mkdir -p "$WORK/sysroot/state"; boot yes
check "first boot: written to /state/config/install-config.yaml" "$SHA" \
    "$(sha256sum "$WORK/sysroot/state/config/install-config.yaml" 2>/dev/null | cut -d' ' -f1)"
check "  0600" 600 "$(stat -c %a "$WORK/sysroot/state/config/install-config.yaml" 2>/dev/null)"
check "  the staged copy is gone before switch_root" no "$([ -e "$WORK/run/install-config.yaml" ] && echo yes || echo no)"
echo "applied: true" > "$WORK/sysroot/state/config/install-config.yaml"
lay; boot yes
check "a node that has one keeps it" "applied: true" "$(cat "$WORK/sysroot/state/config/install-config.yaml")"
check "  and says so" yes "$(said 'already has one')"
check "  the staged copy is gone" no "$([ -e "$WORK/run/install-config.yaml" ] && echo yes || echo no)"
rm -rf "$WORK/sysroot"; mkdir -p "$WORK/sysroot/state"; lay; boot no
check "/state not mounted: nothing written" no "$([ -e "$WORK/sysroot/state/config/install-config.yaml" ] && echo yes || echo no)"
check "  and it says so" yes "$(said 'not mounted')"
check "  the staged copy is gone" no "$([ -e "$WORK/run/install-config.yaml" ] && echo yes || echo no)"
check "no run printed a secret" 0 "$(grep -c 'SECRET-' "$WORK/console" || true)"

[ "$fail" = 0 ] && echo "PASS" || { echo "FAIL"; exit 1; }
