#!/bin/sh
# The link MTU from DHCP option 26, applied before the root connects (#425).
#
# g16's DHCP offers MTU 9000 and every switch, pve and forge run 9000, but the
# Dell booted with stormbr0 at 1500 and its NVMe/TCP root on small frames: the
# udhcpc script never applied $mtu, and udhcpc was never asked for it. Pinned
# here, on the real code (the `dhcp mtu` block of the DHCP script, extracted
# between its markers) against a fake /sys and a stub `ip`:
#   * the lease's MTU goes on the bond's slaves, the bridge's ports and the
#     bridge, in that order, and is said;
#   * a declared `[network] mtu` wins over the lease's;
#   * no MTU in the lease, or one that is not an MTU, changes nothing;
#   * a port that refuses is named, and the bridge still tried;
#   * udhcpc is started with `-O mtu` everywhere.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n "/# --- BEGIN dhcp mtu/,/# --- END dhcp mtu/p" "$GEN" > "$WORK/mtu.sh"
[ -s "$WORK/mtu.sh" ] || { echo "FAIL: could not extract the dhcp mtu block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

# A stub `ip` that records `ip link set X mtu N`, and refuses for the
# interfaces listed in $REFUSE.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/ip" <<'EOF'
#!/bin/sh
if [ "$1 $2" = "link set" ] && [ "$4" = mtu ]; then
    for r in ${REFUSE:-}; do [ "$r" = "$3" ] && exit 2; done
    echo "$3=$5" >> "$IPLOG"
    exit 0
fi
exit 0
EOF
chmod +x "$WORK/bin/ip"

# The /run the block writes to, kept apart per case.
run_case() { # sysfs lease-mtu [declared]
    rm -rf "$WORK/run"; mkdir -p "$WORK/run"
    : > "$WORK/ip.log"
    [ -n "${3:-}" ] && echo "$3" > "$WORK/run/declared-mtu"
    # The block's /run paths, pointed at the test's own.
    sed "s#/run/#$WORK/run/#g" "$WORK/mtu.sh" > "$WORK/mtu-run.sh"
    OUT=$(PATH="$WORK/bin:$PATH" IPLOG="$WORK/ip.log" STORM_SYSFS="$1" sh -c ". '$WORK/mtu-run.sh'; dhcp_mtu stormbr0 '$2'" 2>&1)
    SET=$(tr '\n' ' ' < "$WORK/ip.log" | sed 's/ $//')
}

# stormbr0 over bond0 (eno1, eno2), as a two-port node; and over one port.
S1="$WORK/sys1"
mkdir -p "$S1/class/net/stormbr0/brif/bond0" "$S1/class/net/bond0/bonding" "$S1/class/net/eno1" "$S1/class/net/eno2"
echo "eno1 eno2" > "$S1/class/net/bond0/bonding/slaves"
S2="$WORK/sys2"
mkdir -p "$S2/class/net/stormbr0/brif/enp1s0" "$S2/class/net/enp1s0"

echo "the lease's MTU:"
run_case "$S1" 9000
check "slaves, then the bond, then the bridge" "eno1=9000 eno2=9000 bond0=9000 stormbr0=9000" "$SET"
case "$OUT" in *"mtu 9000 on stormbr0 (from DHCP)"*) echo "  ok    said";; *) echo "  FAIL  not said: $OUT"; fail=1;; esac
check "written down" "9000" "$(cat "$WORK/run/stormblock-mtu" 2>/dev/null)"
run_case "$S2" 9000
check "one port, then the bridge" "enp1s0=9000 stormbr0=9000" "$SET"

echo "a declared [network] mtu wins:"
run_case "$S2" 9000 1500
check "declared over leased" "enp1s0=1500 stormbr0=1500" "$SET"
case "$OUT" in *"(declared [network] mtu)"*) echo "  ok    said";; *) echo "  FAIL  $OUT"; fail=1;; esac
run_case "$S2" "" 9000
check "declared with no lease MTU" "enp1s0=9000 stormbr0=9000" "$SET"

echo "nothing to apply:"
run_case "$S2" ""
check "no option 26: nothing set" "" "$SET"
run_case "$S2" 70000
check "not an MTU: nothing set" "" "$SET"
case "$OUT" in *"not a usable MTU"*) echo "  ok    said";; *) echo "  FAIL  $OUT"; fail=1;; esac
run_case "$S2" abc
check "not a number: nothing set" "" "$SET"

echo "a port that refuses:"
export REFUSE=enp1s0
run_case "$S2" 9000
unset REFUSE
check "the bridge still tried" "stormbr0=9000" "$SET"
case "$OUT" in *"WARNING: enp1s0 does not take mtu 9000"*) echo "  ok    named";; *) echo "  FAIL  $OUT"; fail=1;; esac

echo "udhcpc asks for the MTU:"
n_all=$(grep -c 'udhcpc -i ' "$GEN")
n_mtu=$(grep -c 'udhcpc -i [^ ]* -O mtu ' "$GEN")
check "every udhcpc has -O mtu" "$n_all" "$n_mtu"
grep -q 'dhcp_mtu "$interface" "$mtu"' "$GEN" && echo "  ok    applied on bound/renew" || { echo "  FAIL  not called on bound/renew"; fail=1; }

[ "$fail" = 0 ] && echo "ALL PASS" || { echo "FAILURES"; exit 1; }
