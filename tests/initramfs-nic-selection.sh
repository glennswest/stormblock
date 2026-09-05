#!/bin/sh
# Which uplink the initramfs picks, against a fake /sys/class/net.
#
# The bug this pins: the selection took the first non-loopback interface, so
# on a Dell R230 with a two-port Mellanox it chose eth0 while the cable was in
# eth1, bridged a dead port and stalled in DHCP (stormpump#17). The rule is
# carrier first, then speed — the same one stormbootx applies a stage earlier.
#
# Runs the real code: the selection is extracted from the init script this
# repo generates, between its two marker comments, so the test cannot drift
# from what ships.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN uplink selection/,/# --- END uplink selection/p' "$GEN" > "$WORK/selection.sh"
[ -s "$WORK/selection.sh" ] || { echo "FAIL: could not extract the selection block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

# name:carrier:speed, in the order the kernel would enumerate them
make_tree() {
    root="$WORK/net.$1"; shift
    rm -rf "$root"; mkdir -p "$root"
    # lo has no device link and must never be a candidate
    mkdir -p "$root/lo"; echo 1 > "$root/lo/carrier"
    for spec in "$@"; do
        n=${spec%%:*}; rest=${spec#*:}
        c=${rest%%:*}; s=${rest##*:}
        mkdir -p "$root/$n/device"
        echo "$c" > "$root/$n/carrier"
        echo "$s" > "$root/$n/speed"
        echo "00:00:00:00:00:0$(printf '%s' "$n" | tail -c1)" > "$root/$n/address"
    done
    echo "$root"
}

select_on() { # sysfs root -> prints the ordered candidate list
    (
        NET_SYSFS="$1"
        STORM_LINK_WAIT=1
        export NET_SYSFS STORM_LINK_WAIT
        ip() { :; }            # no interfaces to bring up in a fake tree
        sleep() { :; }         # and no reason to wait for one
        . "$WORK/selection.sh" >/dev/null 2>&1
        echo $CANDIDATES
    )
}

echo "uplink selection:"

# The R230, exactly as it enumerated: two Mellanox ports, cable in the second,
# then two onboard Broadcoms with nothing in them.
t=$(make_tree r230 eth0:0:25000 eth1:1:25000 eth2:0:1000 eth3:0:1000)
check "picks the port with the cable, not the first port" "eth1" "$(select_on "$t")"

# Two live ports: the faster one leads, and the other stays as a fallback.
t=$(make_tree mixed eth0:1:1000 eth1:1:25000)
check "orders live ports fastest first" "eth1 eth0" "$(select_on "$t")"

# A single live port is just itself.
t=$(make_tree single eth0:1:1000)
check "one live port" "eth0" "$(select_on "$t")"

# Nothing reports carrier: try them all rather than refuse to try.
t=$(make_tree dead eth0:0:1000 eth1:0:1000)
check "no carrier anywhere falls back to every port" "eth0 eth1" "$(select_on "$t")"

# lo is not a port, and neither is a bridge (no device link).
t=$(make_tree virt eth0:1:1000)
mkdir -p "$WORK/net.virt/stormbr0"; echo 1 > "$WORK/net.virt/stormbr0/carrier"
check "skips loopback and bridges" "eth0" "$(select_on "$WORK/net.virt")"

# A port whose speed is unreadable (down, or a driver that does not report it)
# sorts last rather than breaking the sort.
t=$(make_tree nospeed eth0:1:1000 eth1:1:0)
rm -f "$WORK/net.nospeed/eth1/speed"
check "unreadable speed sorts last, does not break ordering" "eth0 eth1" "$(select_on "$t")"

[ "$fail" -eq 0 ] && echo "all uplink selection checks passed"
exit "$fail"
