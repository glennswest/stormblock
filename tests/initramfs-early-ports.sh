#!/bin/sh
# Ports up early, raised to a jumbo MTU while down (#545).
#
# server8 (X9, mlx4) reboot, 12.94: its ports went up only at the uplink
# selection, after the node state read, so DHCP waited out the switch (five
# discovers, ~3.5 s); then the lease's MTU 9000 reset eth1's link, the three
# DNS tries for its name were lost (3 s) and the node was named after its MAC.
# Pinned here, on the real code (the `early ports` and `bridge bring-up`
# blocks and path.script, extracted between their markers), against a fake
# /sys and a stub `ip`:
#   * every physical port is brought up; lo and a device-less link are not;
#   * a port still down and below PORT_MTU is raised before it goes up, its
#     own MTU kept; an up port, one already at it, `off`, a bond: not raised;
#   * a refused raise keeps nothing;
#   * the bridge is pinned to the port's own MTU; an address on the port
#     itself (no bridge) puts the port's MTU back;
#   * a path NIC lease with no MTU puts the port's own back; one with an MTU
#     sets that.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN early ports/,/# --- END early ports/p' "$GEN" | grep -v '^early_ports$' > "$WORK/early.sh"
[ -s "$WORK/early.sh" ] || { echo "FAIL: could not extract the early ports block"; exit 1; }
sed -n '/# --- BEGIN bridge bring-up/,/# --- END bridge bring-up/p' "$GEN" > "$WORK/bridge.sh"
[ -s "$WORK/bridge.sh" ] || { echo "FAIL: could not extract the bridge bring-up block"; exit 1; }
sed -n "/cat > \"\$INITRD_DIR\/usr\/share\/udhcpc\/path.script\" << 'PATHSCRIPT'/,/^PATHSCRIPT\$/p" "$GEN" | sed '1d;$d' > "$WORK/path.script"
[ -s "$WORK/path.script" ] || { echo "FAIL: could not extract path.script"; exit 1; }
grep -q '^early_ports$' "$GEN" && echo "  ok    early_ports is called" || { echo "  FAIL  early_ports is never called"; exit 1; }

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
    case "$3" in *"$2"*) echo "  ok    $1" ;; *) echo "  FAIL  $1: '$2' not in: $3"; fail=1 ;; esac
}

# A stub `ip`: logs every call; `link set X mtu N` refused for $REFUSE.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/ip" <<'EOF'
#!/bin/sh
if [ "$1 $2" = "link set" ] && [ "${4:-}" = mtu ]; then
    for r in ${REFUSE:-}; do [ "$r" = "$3" ] && exit 2; done
fi
echo "$*" >> "$IPLOG"
exit 0
EOF
chmod +x "$WORK/bin/ip"
cat > "$WORK/bin/modprobe" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$WORK/bin/modprobe"

# server8's ports: eth0 (ixgbe, down), eth1 (mlx4, down), lo, a veth with no
# device, and eth2 already up.
mksys() {
    S="$WORK/sys"; rm -rf "$S"
    for p in eth0 eth1 eth2; do mkdir -p "$S/class/net/$p/device"; echo 1500 > "$S/class/net/$p/mtu"; echo 0x1002 > "$S/class/net/$p/flags"; done
    echo 0x1003 > "$S/class/net/eth2/flags"
    mkdir -p "$S/class/net/lo" "$S/class/net/veth0"
    echo 65536 > "$S/class/net/lo/mtu"; echo 1500 > "$S/class/net/veth0/mtu"
}

run_early() { # env assignments...
    rm -rf "$WORK/run"; : > "$WORK/ip.log"
    OUT=$(env PATH="$WORK/bin:$PATH" IPLOG="$WORK/ip.log" STORM_SYSFS="$WORK/sys" \
          STORM_PORT_MTU_RUN="$WORK/run" "$@" sh -c ". '$WORK/early.sh'; early_ports" 2>&1)
    LOG=$(tr '\n' ';' < "$WORK/ip.log")
}

echo "early ports, server8's shape:"
mksys
run_early
check "down ports raised before up, every port up, lo and veth left" \
    "link set eth0 mtu 9000;link set eth0 up;link set eth1 mtu 9000;link set eth1 up;link set eth2 up;" "$LOG"
check "eth1's own MTU kept" "1500" "$(cat "$WORK/run/eth1" 2>/dev/null)"
check "an up port keeps nothing" "no" "$([ -e "$WORK/run/eth2" ] && echo yes || echo no)"
contains "the ports said" "ports up: eth0 eth1 eth2" "$OUT"
contains "the raise said" "mtu 9000 on eth0 eth1 while down" "$OUT"

echo "not raised:"
mksys; echo 9000 > "$WORK/sys/class/net/eth0/mtu"
run_early
contains "a port already at 9000" "link set eth0 up;link set eth1 mtu 9000" "$LOG"
mksys
run_early PORT_MTU=off
check "rd.stormblock.port-mtu=off" "link set eth0 up;link set eth1 up;link set eth2 up;" "$LOG"
run_early BOND_MODE=802.3ad
check "a bond: nothing raised" "link set eth0 up;link set eth1 up;link set eth2 up;" "$LOG"
run_early PORT_MTU=4000
contains "rd.stormblock.port-mtu=4000" "link set eth0 mtu 4000;" "$LOG"
REFUSE=eth1 run_early
check "a refused raise keeps nothing" "no" "$([ -e "$WORK/run/eth1" ] && echo yes || echo no)"
contains "and is not said as raised" "mtu 9000 on eth0 while down" "$OUT"

# The bridge: the real block, its /sys/class/net pointed at the fake tree.
run_bridge() { # port [NO_BRIDGE]
    : > "$WORK/ip.log"
    sed "s#/sys/class/net/#$WORK/sys/class/net/#g" "$WORK/bridge.sh" > "$WORK/bridge-run.sh"
    OUT=$(env PATH="$WORK/bin:$PATH" IPLOG="$WORK/ip.log" STORM_SYSFS="$WORK/sys" \
          STORM_PORT_MTU_RUN="$WORK/run" NO_BRIDGE="${2:-}" BRIDGE=stormbr0 \
          sh -c "netsay() { echo \"\$*\"; }; . '$WORK/early.sh'; . '$WORK/bridge-run.sh'; net_bring_up $1; echo IFACE=\$IFACE" 2>&1)
    LOG=$(tr '\n' ';' < "$WORK/ip.log")
}
echo "the bridge:"
mksys; run_early
mkdir -p "$WORK/sys/class/net/stormbr0"
run_bridge eth1
contains "the bridge pinned at the port's own MTU" "link set eth1 master stormbr0;link set stormbr0 up;link set stormbr0 mtu 1500;" "$LOG"
contains "the address on the bridge" "IFACE=stormbr0" "$OUT"
check "the port stays raised" "1500" "$(cat "$WORK/run/eth1" 2>/dev/null)"
run_bridge eth2
case "$LOG" in *"stormbr0 mtu"*) echo "  FAIL  a port never raised pins the bridge: $LOG"; fail=1 ;; *) echo "  ok    a port never raised: the bridge follows it" ;; esac
echo "no bridge:"
mksys; run_early
run_bridge eth1 1
contains "the port's own MTU back" "link set eth1 mtu 1500" "$LOG"
check "and forgotten" "no" "$([ -e "$WORK/run/eth1" ] && echo yes || echo no)"
contains "said" "mtu 1500 back on eth1" "$OUT"

echo "a path NIC's lease:"
run_path() { # mtu
    : > "$WORK/ip.log"
    env PATH="$WORK/bin:$PATH" IPLOG="$WORK/ip.log" STORM_RUN="$WORK/prun" STORM_PORT_MTU_RUN="$WORK/run" \
        interface=eth0 ip=192.168.16.200 mask=20 mtu="$1" sh "$WORK/path.script" bound >/dev/null 2>&1
    LOG=$(tr '\n' ';' < "$WORK/ip.log")
}
mksys; run_early
run_path ""
contains "no lease MTU: the port's own back" "link set eth0 mtu 1500" "$LOG"
check "and forgotten" "no" "$([ -e "$WORK/run/eth0" ] && echo yes || echo no)"
mksys; run_early
run_path 9000
contains "a lease MTU: set" "link set eth0 mtu 9000" "$LOG"
case "$LOG" in *"mtu 1500"*) echo "  FAIL  the own MTU put back over the lease's: $LOG"; fail=1 ;; *) echo "  ok    not the own MTU" ;; esac

[ "$fail" = 0 ] && echo "ALL PASS" || { echo "FAILURES"; exit 1; }
