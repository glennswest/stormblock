#!/bin/sh
# The boot NIC's address: what the node declares wins over `ip=dhcp` (#229).
#
# Every release's command line says `ip=dhcp` (one line boots every node,
# stormcos#182), and stormpump never addresses a bridge port after
# switch_root (stormpump#33). So a node that declared a static address for
# its boot NIC in `[network]` (stormcos.toml, or install-node.toml from
# install-config.yaml, #78) still took a lease, and if a server answered, the
# lease won. Pinned here:
#   * the node state read: the local disk's stormcos-state found before the
#     network, by the disk the command line names or (none named) by looking,
#     never a removable drive or one in a shelf;
#   * the choice of file: stormcos.toml when it declares any interface, else
#     install-node.toml (stormpump's `plan_for`);
#   * static on an exact, present port with carrier -> that port, its
#     addresses, gateway, dns, domain, mtu; no carrier, absent, a pattern, no
#     prefix -> DHCP as before, and said;
#   * dhcp on a named port -> that port first;
#   * a static `ip=` on the command line wins, and its <device> is honoured;
#   * rd.stormblock.declared-net=off ignores the declaration.
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
extract "node state read" state.sh
extract "uplink selection" select.sh
extract "boot nic" nic.sh

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
echo "node state read:"

# `stormblock slab cat` against $STUB_ROOT/<disk>/<file>: a directory for the
# disk means it holds stormcos-state (exit 1 for a file it lacks), none means
# it does not (exit 2).
STUB="$WORK/stormblock"
cat > "$STUB" <<'STUBEOF'
#!/bin/sh
[ "$1 $2" = "slab cat" ] || exit 2
shift 2
slab=""; out=""; vol=""
while [ $# -gt 1 ]; do
    case "$1" in
    --slab) slab="$2"; shift ;;
    --out) out="$2"; shift ;;
    --volume) vol="$2"; shift ;;
    esac
    shift
done
path="$1"
echo "$slab" >> "$STUB_ROOT/asked"
d="$STUB_ROOT/${slab##*/}"
[ "$vol" = stormcos-state ] && [ -d "$d" ] || exit 2
f="$d/${path##*/}"
[ -f "$f" ] || exit 1
cp "$f" "$out"
STUBEOF
chmod +x "$STUB"

# A fake /sys/block: sda (internal, nothing on it), sdb (removable, holds a
# state), sdc (in a shelf, holds a state), nvme0n1 (internal, holds a state).
SYS="$WORK/sys"
mkdir -p "$SYS/sda" "$SYS/sdb" "$SYS/nvme0n1" "$WORK/expander-0:0/sdc/device/enclosure_device:Slot 01"
echo 0 > "$SYS/sda/removable"; echo 1 > "$SYS/sdb/removable"; echo 0 > "$SYS/nvme0n1/removable"
echo 0 > "$WORK/expander-0:0/sdc/removable"
ln -s "$WORK/expander-0:0/sdc" "$SYS/sdc"

state() ( # -> STATE_DISK|files read ; console in state.out
    set +e
    STORM_STORMBLOCK="$STUB"; STORM_RUN="$WORK/run"; STORM_STATE_SYS="$SYS"; STORM_STATE_DEV="/dev"
    STUB_ROOT="$WORK/root"; export STUB_ROOT
    rm -rf "$WORK/run" "$WORK/root"; mkdir -p "$WORK/run" "$WORK/root"
    for spec in ${HAS:-}; do # disk:file
        mkdir -p "$WORK/root/${spec%%:*}"
        [ "${spec#*:}" = "-" ] || echo "from ${spec%%:*}" > "$WORK/root/${spec%%:*}/${spec#*:}"
    done
    SLAB="${SLAB_ARG:-}"; ALLOW_EXTERNAL="${EXT:-}"
    . "$WORK/state.sh" > "$WORK/state.out" 2>&1
    echo "$STATE_DISK|$(cd "$STATE_DIR" && ls | tr '\n' ' ')"
)

check "no disk named: the first internal disk that holds it" \
    "/dev/nvme0n1|install-node.toml stormcos.toml " \
    "$(HAS="nvme0n1:stormcos.toml nvme0n1:install-node.toml sdb:stormcos.toml sdc:stormcos.toml" state)"
contains "and the console says where" "stormcos-state on /dev/nvme0n1" "$(cat "$WORK/state.out")"
check "a removable drive and a shelf drive are never read" "|" \
    "$(HAS="sdb:stormcos.toml sdc:stormcos.toml" state)"
asked=$(cat "$WORK/root/asked" 2>/dev/null | tr '\n' ' ')
check "  (only sda and nvme0n1 were asked)" "/dev/sda /dev/nvme0n1 " "$asked"
check "rd.stormblock.allow-external=1: a shelf drive is" "/dev/sdc|stormcos.toml " \
    "$(HAS="sdc:stormcos.toml" EXT=1 state)"
check "the named disk, and only it" "|" \
    "$(SLAB_ARG=/dev/nope HAS="nvme0n1:stormcos.toml" state)"
mkdir -p "$WORK/named"; : > "$WORK/named/sdz"
check "the named disk read" "$WORK/named/sdz|stormcos.toml " \
    "$(SLAB_ARG="$WORK/named/sdz" HAS="sdz:stormcos.toml nvme0n1:install-node.toml" state)"
check "a fabric URI: nothing read before the network" "|" \
    "$(SLAB_ARG=nvme-tcp://10.0.0.1:4420/nqn HAS="nvme0n1:stormcos.toml" state)"
check "the volume with neither file: found, nothing copied" "/dev/nvme0n1|" \
    "$(HAS="nvme0n1:-" state)"
check "install-node.toml alone" "/dev/nvme0n1|install-node.toml " \
    "$(HAS="nvme0n1:install-node.toml" state)"

# ---------------------------------------------------------------------------
echo "boot nic:"

# name:carrier:speed, as in tests/initramfs-nic-selection.sh
make_tree() {
    root="$WORK/net"
    rm -rf "$root"; mkdir -p "$root/lo"
    for spec in "$@"; do
        n=${spec%%:*}; rest=${spec#*:}
        mkdir -p "$root/$n/device"
        echo "${rest%%:*}" > "$root/$n/carrier"
        echo "${rest##*:}" > "$root/$n/speed"
    done
}

# One run: TOML / NODE are the two files' contents (unset: absent), IPC the
# command line's ip=. Prints NIC|ADDRS|GW|DNS|DOMAIN|MTU|CANDIDATES|IFACE.
nic() (
    set +e
    NET_SYSFS="$WORK/net"; STORM_LINK_WAIT=1
    ip() { :; }; sleep() { :; }
    . "$WORK/select.sh" > /dev/null 2>&1
    netsay() { echo "$@"; }
    STATE_DIR="$WORK/state"; rm -rf "$STATE_DIR"; mkdir -p "$STATE_DIR"
    STATE_TOML="$STATE_DIR/stormcos.toml"; STATE_NODE="$STATE_DIR/install-node.toml"
    STATE_DISK=/dev/sda
    [ -n "${TOML+x}" ] && printf '%s\n' "$TOML" > "$STATE_TOML"
    [ -n "${NODE+x}" ] && printf '%s\n' "$NODE" > "$STATE_NODE"
    IP_CONF="${IPC-dhcp}"; DECLARED_NET="${DN:-}"
    . "$WORK/nic.sh" > "$WORK/nic.out" 2>&1
    echo "$BOOT_FROM" > "$WORK/nic.from"
    echo "$BOOT_NIC|$BOOT_ADDRS|$BOOT_GW|$BOOT_DNS|$BOOT_DOMAIN|$BOOT_MTU|$(echo $CANDIDATES)|$IFACE"
)

make_tree eth0:1:10000 eth1:1:1000 eth2:0:25000

STATIC='[node]
hostname = "stormblock1"

[network]
interface = "eth1"   # the onboard port
mode = "static"
address = "192.168.16.20/24"
gateway = "192.168.16.1"
dns = ["192.168.16.252", "192.168.1.252"]
domain = "g16.lo"
mtu = 9000

[[network.interfaces]]
name = "eth0"
mode = "dhcp"'

check "stormcos.toml static on eth1: eth1, everything it states" \
    "eth1|192.168.16.20/24|192.168.16.1|192.168.16.252 192.168.1.252|g16.lo|9000|eth0 eth1|eth0" \
    "$(TOML="$STATIC" nic)"
check "no declaration: DHCP as before" "||||||eth0 eth1|eth0" "$(nic)"

NODE_STATIC='# Written by stormpump from install-config.yaml (#78)

[node]
hostname = "stormblock1"

[network]
interface = "eth1"
mode = "static"
address = "10.1.0.5/16"
gateway = "10.1.0.1"
dns = ["10.1.0.2"]'
check "install-node.toml when stormcos.toml declares nothing" \
    "eth1|10.1.0.5/16|10.1.0.1|10.1.0.2|||eth0 eth1|eth0" \
    "$(TOML='[node]
hostname = "x"
[network]
mode = "dhcp"' NODE="$NODE_STATIC" nic)"
NODE="$NODE_STATIC" nic > /dev/null
check "  and says which file" "[network] in install-node.toml on /dev/sda" "$(cat "$WORK/nic.from")"
TOML="$STATIC" nic > /dev/null
check "  (stormcos.toml when it declares)" "[network] in stormcos.toml on /dev/sda" "$(cat "$WORK/nic.from")"
check "stormcos.toml declaring only [[network.interfaces]] wins, whole: no static" \
    "||||||eth0 eth1|eth0" \
    "$(TOML='[[network.interfaces]]
driver = "mlx4_en"
mode = "dhcp"' NODE="$NODE_STATIC" nic)"
check "an address in another section is not [network]'s" "||||||eth0 eth1|eth0" \
    "$(TOML='[network]
interface = "eth1"
mode = "static"
[node.network]
address = "10.9.9.9/24"' nic)"
check "addresses as a list" "eth1|10.0.0.5/24 10.0.0.6/24|||||eth0 eth1|eth0" \
    "$(TOML='[network]
name = "eth1"
mode = "static"
addresses = ["10.0.0.5/24", "10.0.0.6/24"]' nic)"

r=$(TOML='[network]
interface = "eth2"
mode = "static"
address = "10.0.0.5/24"' nic)
check "static on a port with no carrier: DHCP on the others" "||||||eth0 eth1|eth0" "$r"
contains "  and says so" "eth2, which has no carrier" "$(cat "$WORK/nic.out")"
r=$(TOML='[network]
interface = "eno1"
mode = "static"
address = "10.0.0.5/24"' nic)
check "a port this machine does not have: DHCP" "||||||eth0 eth1|eth0" "$r"
contains "  and says so" "eno1, which this machine does not have" "$(cat "$WORK/nic.out")"
r=$(TOML='[network]
interface = "eth*"
mode = "static"
address = "10.0.0.5/24"' nic)
check "a pattern is not one port" "||||||eth0 eth1|eth0" "$r"
r=$(TOML='[network]
interface = "eth1"
mode = "static"
address = "10.0.0.5"' nic)
check "an address with no prefix is not applied" "||||||eth0 eth1|eth0" "$r"
contains "  and says so" "states no prefix" "$(cat "$WORK/nic.out")"
check "mode up: nothing for the boot to do" "||||||eth0 eth1|eth0" \
    "$(TOML='[network]
interface = "eth1"
mode = "up"' nic)"
check "dhcp on a named port: that port first" "||||||eth1 eth0|eth1" \
    "$(TOML='[network]
interface = "eth1"
mode = "dhcp"' nic)"
check "dhcp on a named port with no carrier: as before" "||||||eth0 eth1|eth0" \
    "$(TOML='[network]
interface = "eth2"' nic)"
r=$(TOML="$STATIC" DN=off nic)
check "rd.stormblock.declared-net=off: ignored" "||||||eth0 eth1|eth0" "$r"
contains "  and says so" "declared-net=off" "$(cat "$WORK/nic.out")"

r=$(TOML="$STATIC" IPC='10.5.0.9::10.5.0.1:255.255.0.0::eth1:none' nic)
check "a static ip= wins, on its <device>, the mask made a prefix" \
    "eth1|10.5.0.9/16|10.5.0.1||||eth0 eth1|eth0" "$r"
contains "  and says the declaration lost" "wins over the declaration" "$(cat "$WORK/nic.out")"
check "a static ip= with no <device>: the selected port" "eth0|10.5.0.9/24|10.5.0.1||||eth0 eth1|eth0" \
    "$(IPC='10.5.0.9::10.5.0.1:24::' nic)"
r=$(IPC='10.5.0.9::10.5.0.1:24::eno9:none' nic)
check "a static ip= naming a port that is not here: the selected one" \
    "eth0|10.5.0.9/24|10.5.0.1||||eth0 eth1|eth0" "$r"
contains "  and says so" "ip= names eno9" "$(cat "$WORK/nic.out")"
check "ip=off is no static address" "||||||eth0 eth1|eth0" "$(IPC=off nic)"

[ "$fail" = 0 ] && echo "all boot nic checks passed" || { echo "FAILURES"; exit 1; }
