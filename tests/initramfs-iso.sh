#!/bin/sh
# An install from an attached release ISO (#442): /init's claim is the
# flavour's disk on the ISO, `stormdist://<device>?flavour=<f>&key=<file>`,
# never read without the release key built into the image. The flavour is
# the install-config's networking.networkType (cilium when none); `auto`
# takes the first CD with media.
#
# Runs the real code: the iso claim block is extracted from the init script
# this repo generates, between its marker comments.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

BLOCK="$WORK/iso-claim.sh"
sed -n "/# --- BEGIN iso claim/,/# --- END iso claim/p" "$GEN" > "$BLOCK"
[ -s "$BLOCK" ] || { echo "FAIL: could not extract the iso claim block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

KEY="$WORK/release-key.pub"
SYSB="$WORK/sys/block"
mkdir -p "$SYSB"
claim() { # ISO [install-config] -> CLAIMED, or "refused"; console in $WORK/console
    ISO="$1" INSTALL_CONFIG_STAGED="${2:-}" STORM_RELEASE_KEY="$KEY" STORM_SYS_BLOCK="$SYSB" STORM_DEV=/dev sh -c '
        CLAIMED=""
        . "$0"
        if iso_claim; then echo "CLAIMED=$CLAIMED"; else echo "CLAIMED=refused"; fi
    ' "$BLOCK" > "$WORK/console" 2>&1 || true
    sed -n 's/^CLAIMED=//p' "$WORK/console"
}
said() { grep -q "$1" "$WORK/console" && echo yes || echo no; }

echo "the release key:"
rm -f "$KEY"
check "no key built in: refused" refused "$(claim /dev/sr0)"
check "  and said" yes "$(said 'no release key')"
echo 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef > "$KEY"

echo "the device:"
check "named" "stormdist:///dev/sr0?flavour=cilium&key=$KEY" "$(claim /dev/sr0)"
check "  the ISO stays attached is said" yes "$(said 'stays attached until the flow-over ends')"
mkdir -p "$SYSB/sr0" "$SYSB/sr1"; echo 0 > "$SYSB/sr0/size"; echo 2097152 > "$SYSB/sr1/size"
check "auto: the first CD with media" "stormdist:///dev/sr1?flavour=cilium&key=$KEY" "$(claim auto)"
echo 0 > "$SYSB/sr1/size"
check "auto with no media: refused" refused "$(claim auto)"
check "  and the way out said" yes "$(said 'rd.stormblock.iso=/dev/sdX')"

echo "the flavour:"
printf 'pullSecret: SECRET\nnetworking:\n  networkType: flowsdn   # the flavor\n' > "$WORK/ic1.yaml"
check "networking.networkType" "stormdist:///dev/sdb?flavour=flowsdn&key=$KEY" "$(claim /dev/sdb "$WORK/ic1.yaml")"
printf 'networking:\n  networkType: "cilium"\n' > "$WORK/ic2.yaml"
check "quoted" "stormdist:///dev/sdb?flavour=cilium&key=$KEY" "$(claim /dev/sdb "$WORK/ic2.yaml")"
printf 'other:\n  networkType: flowsdn\nnetworking:\n  machineNetwork: x\n' > "$WORK/ic3.yaml"
check "only under networking: (else cilium)" "stormdist:///dev/sdb?flavour=cilium&key=$KEY" "$(claim /dev/sdb "$WORK/ic3.yaml")"
check "no secret printed" 0 "$(grep -c SECRET "$WORK/console" || true)"

[ "$fail" = 0 ] && echo "PASS" || { echo "FAIL"; exit 1; }
