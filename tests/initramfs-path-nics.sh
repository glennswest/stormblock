#!/bin/sh
# Every other port that is up gets an address for the flow-over's NVMe/TCP
# paths (#450, the master's A): not the uplink, not a bond or bridge port, not
# one rd.stormblock.path-skip= names, only one with carrier; ARP each for its
# own address; and the lease handed over in a file, since nothing in the
# initramfs renews it after switch_root.
#
# Runs the real code: the block and the DHCP script are extracted from the
# init script this repo generates, so the test cannot drift.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN path nics/,/# --- END path nics/p' "$GEN" > "$WORK/block.sh"
[ -s "$WORK/block.sh" ] || { echo "FAIL: could not extract the path nics block"; exit 1; }
sed -n "/cat > \"\$INITRD_DIR\/usr\/share\/udhcpc\/path.script\" << 'PATHSCRIPT'/,/^PATHSCRIPT\$/p" "$GEN" | sed '1d;$d' > "$WORK/path.script"
[ -s "$WORK/path.script" ] || { echo "FAIL: could not extract path.script"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

# The ports: eth0 the uplink (a port of stormbr0), eth1 cabled, eth2 no cable,
# eth3 cabled and skipped, eth4 a bond port, eth5 cabled with no DHCP answer.
NS="$WORK/net"
for p in eth0:1 eth1:1 eth2:0 eth3:1 eth4:1 eth5:1; do
    n=${p%%:*}
    mkdir -p "$NS/$n/device"
    echo "${p#*:}" > "$NS/$n/carrier"
done
ln -s ../stormbr0 "$NS/eth0/master" 2>/dev/null || mkdir -p "$NS/eth0/master"
mkdir -p "$NS/eth4/master"
SC="$WORK/sysctl"
for f in all eth1 eth5 stormbr0; do mkdir -p "$SC/$f"; echo 0 > "$SC/$f/arp_ignore"; echo 0 > "$SC/$f/arp_announce"; done

run() { # extra environment -> console in $WORK/out, leased ports in $WORK/asked
    : > "$WORK/asked"
    (
        NET_SYSFS="$NS"; STORM_SYSCTL_NET="$SC"; STORM_PATH_SCRIPT="$WORK/path.script"
        ALL_PHYS="eth0 eth1 eth2 eth3 eth4 eth5"; LEASED=eth0; IFACE=stormbr0
        STORMBLOCK_PATH_SKIP="eth3"
        for kv in "$@"; do eval "$kv"; done
        netsay() { echo "$@"; }
        net_carrier() { cat "$NET_SYSFS/$1/carrier" 2>/dev/null || echo 0; }
        udhcpc() {
            while [ $# -gt 0 ]; do case "$1" in -i) _i=$2; shift ;; -x) echo "HOSTNAME SENT" ;; esac; shift; done
            echo "$_i" >> "$WORK/asked"
            [ "$_i" != eth5 ]
        }
        ip() { [ "$1 $2 $3" = "-4 -o addr" ] && echo "2: $6 inet 10.20.0.12/24 brd 10.20.0.255 scope global $6"; return 0; }
        . "$WORK/block.sh"
        echo "PATH_NICS=$PATH_NICS"
    ) > "$WORK/out" 2>&1
}
said() { grep -q "$1" "$WORK/out" && echo yes || echo no; }

echo "the block:"
run
check "only the cabled, unbonded, unskipped ports are asked" "eth1 eth5" "$(tr '\n' ' ' < "$WORK/asked" | sed 's/ $//')"
check "the one that got a lease is a path NIC" " eth1" "$(sed -n 's/^PATH_NICS=//p' "$WORK/out")"
check "  and says its address" yes "$(said 'path NIC eth1: 10.20.0.12/24')"
check "a port with no answer is said, not used" yes "$(said 'path NIC eth5: no lease - not used')"
check "the skipped one is said" yes "$(said 'path NIC eth3: skipped')"
check "no hostname is sent for a path NIC" no "$(said 'HOSTNAME SENT')"
check "arp_ignore=1 on all" 1 "$(cat "$SC/all/arp_ignore")"
check "  arp_announce=2 on the path NIC" 2 "$(cat "$SC/eth1/arp_announce")"
check "  and on the uplink's bridge" 1 "$(cat "$SC/stormbr0/arp_ignore")"
run 'PATH_NICS_MODE=off'
check "rd.stormblock.path-nics=off: nothing asked" "" "$(cat "$WORK/asked")"
check "  and said" yes "$(said 'path NICs: off')"
run 'ALL_PHYS="eth0"'
check "a one-port node: nothing asked" "" "$(cat "$WORK/asked")"

echo "the DHCP script:"
LOG="$WORK/ip.log"
: > "$LOG"
mkdir -p "$WORK/bin"
cat > "$WORK/bin/ip" <<EOF
#!/bin/sh
echo "ip \$*" >> "$LOG"
[ "\$1 \$2 \$3" = "route show dev" ] && echo "10.20.0.0/24 scope link  src 10.20.0.12"
exit 0
EOF
chmod +x "$WORK/bin/ip"
PATH="$WORK/bin:$PATH" STORM_RUN="$WORK/run" interface=eth1 ip=10.20.0.12 mask=24 serverid=10.20.0.252 lease=600 mtu=9000 \
    sh "$WORK/path.script" bound
check "the address" yes "$(grep -q 'ip addr add 10.20.0.12/24 dev eth1' "$LOG" && echo yes || echo no)"
check "its subnet route re-added below the uplink's" yes "$(grep -q 'ip route add 10.20.0.0/24 dev eth1 src 10.20.0.12 metric 1024' "$LOG" && echo yes || echo no)"
check "no default route" no "$(grep -q 'default' "$LOG" && echo yes || echo no)"
check "the MTU it was offered" yes "$(grep -q 'ip link set eth1 mtu 9000' "$LOG" && echo yes || echo no)"
H="$WORK/run/path-nics/eth1.json"
check "the lease handed over" yes "$([ -s "$H" ] && echo yes || echo no)"
if command -v python3 >/dev/null 2>&1; then
    check "  as JSON stormpump can renew from" "eth1 10.20.0.12 24 10.20.0.252 600" \
        "$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["interface"], d["address"], d["prefix"], d["server"], d["lease_secs"])' "$H")"
fi
PATH="$WORK/bin:$PATH" STORM_RUN="$WORK/run" interface=eth1 sh "$WORK/path.script" deconfig
check "deconfig drops the handover" no "$([ -e "$H" ] && echo yes || echo no)"

[ "$fail" = 0 ] && echo "PASS" || { echo "FAIL"; exit 1; }
