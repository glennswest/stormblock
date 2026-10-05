#!/bin/sh
# How /init names the node, and what it asks DHCP under (#238, stormcos#191).
#
# C2NR0Q2 registered as `storm-06f96d` although its reservation says
# `stormblock1` and its PTR, forward-confirmed, says the same: microdns sent
# no option 12, and with no DNS server in the lease the PTR step was skipped
# without a word. Pinned here:
#   * the chain: option 12, then a forward-confirmed PTR, then storm-<mac>,
#     each step saying why it gave no name;
#   * the FQDN from the lease's domain (or the name's own), set as the
#     kernel's domainname;
#   * the name the node asks DHCP under: its declared name (stormcos.toml on
#     the local disk), else the name its firmware booted as; never a guess.
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
extract "node name" name.sh
extract "dhcp name hint" hint.sh

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
echo "node name:"

# One run of the naming block. The lease is described by:
#   OPT12   option 12 (empty: none)    DOMAIN  option 15/119 (empty: none)
#   DNS     nameservers (empty: none)  PTR     what DNS calls 192.168.30.2
#   BACK    what the PTR name resolves to (space-separated)
# Prints NODE_NAME|hostname written|domainname written; console in name.out.
name() (
    set +e
    R="$WORK/run.$$"; rm -rf "$R"; mkdir -p "$R/run" "$R/sys/kernel" "$R/net/eth0"
    [ -n "${OPT12:-}" ] && echo "$OPT12" > "$R/run/dhcp-hostname"
    [ -n "${DOMAIN:-}" ] && echo "$DOMAIN" > "$R/run/dhcp-domain"
    : > "$R/resolv.conf"
    [ -n "${DOMAIN:-}" ] && echo "search $DOMAIN" >> "$R/resolv.conf"
    for d in ${DNS:-}; do echo "nameserver $d" >> "$R/resolv.conf"; done
    echo "0c:c4:7a:06:f9:6d" > "$R/net/eth0/address"
    STORM_NAME_RUN="$R/run"; STORM_RESOLV="$R/resolv.conf"
    STORM_PROC_SYS="$R/sys"; STORM_SYSNET="$R/net"
    IFACE=eth0; UPLINK=eth0
    ip() { echo "2: eth0    inet 192.168.30.2/20 brd 192.168.31.255 scope global eth0"; }
    nslookup() {
        case "$1" in
        192.168.30.2) [ -n "${PTR:-}" ] && printf 'Server: 192.168.31.252\n\n2.30.168.192.in-addr.arpa\tname = %s.\n' "$PTR" ;;
        *) for a in ${BACK:-}; do printf 'Address: %s\n' "$a"; done ;;
        esac
        return 0
    }
    . "$WORK/name.sh" > "$WORK/name.out" 2>&1
    echo "$NODE_NAME|$(cat "$R/sys/kernel/hostname" 2>/dev/null)|$(cat "$R/sys/kernel/domainname" 2>/dev/null)"
)

r=$(OPT12=stormblock1 DOMAIN=g16.lo DNS=192.168.31.252 name)
check "option 12 names the node, with the lease's domain" "stormblock1|stormblock1|g16.lo" "$r"
contains "and the console says where the name came from" "name from DHCP: stormblock1 (option 12)" "$(cat "$WORK/name.out")"
contains "and states the FQDN" "fqdn: stormblock1.g16.lo" "$(cat "$WORK/name.out")"

r=$(OPT12=stormblock1.g16.lo name)
check "an FQDN in option 12: the short name, its domain the FQDN's" "stormblock1|stormblock1|g16.lo" "$r"

# C2NR0Q2: no option 12 (microdns#14), a confirmed PTR, two A records.
r=$(DOMAIN=g16.lo DNS=192.168.31.252 PTR=stormblock1.g16.lo BACK="192.168.30.1 192.168.30.2" name)
check "no option 12: the confirmed PTR names it" "stormblock1|stormblock1|g16.lo" "$r"
contains "and the missing option 12 is said" "the lease names no host (no option 12)" "$(cat "$WORK/name.out")"

r=$(DNS=192.168.31.252 PTR=stormblock1.g16.lo BACK="192.168.30.2" name)
check "with no domain in the lease, the PTR's domain is the FQDN's" "stormblock1|stormblock1|g16.lo" "$r"

r=$(DOMAIN=g16.lo PTR=stormblock1.g16.lo BACK="192.168.30.2" name)
check "no DNS server in the lease: the MAC" "storm-06f96d|storm-06f96d|g16.lo" "$r"
contains "and why the PTR was not asked is said" "no DNS server in the lease: cannot ask DNS what 192.168.30.2 is called" \
    "$(cat "$WORK/name.out")"
contains "and that the name was made up" "nothing named this node: storm-06f96d" "$(cat "$WORK/name.out")"

r=$(DOMAIN=g16.lo DNS=192.168.31.252 name)
check "no PTR: the MAC" "storm-06f96d|storm-06f96d|g16.lo" "$r"
contains "and the missing PTR is said" "DNS has no name for 192.168.30.2 (no PTR record from 192.168.31.252)" \
    "$(cat "$WORK/name.out")"

# The stale PTR this pool had: a Windows box's name, whose A is another address.
r=$(DOMAIN=g16.lo DNS=192.168.31.252 PTR=minint-fsmpc1o.g16.lo BACK="192.168.30.9" name)
check "a PTR that does not resolve back is ignored" "storm-06f96d|storm-06f96d|g16.lo" "$r"
contains "and said" "ignoring" "$(cat "$WORK/name.out")"

r=$(name)
check "nothing at all: the MAC, no domain" "storm-06f96d|storm-06f96d|" "$r"

# ---------------------------------------------------------------------------
echo "the name DHCP is asked under:"

STUB="$WORK/stormblock"
cat > "$STUB" <<'STUBEOF'
#!/bin/sh
[ "$1 $2" = "slab cat" ] || exit 2
out=""; vol=""
while [ $# -gt 0 ]; do
    case "$1" in --out) out="$2"; shift ;; --volume) vol="$2"; shift ;; esac
    shift
done
[ "$vol" = stormcos-state ] && [ -n "${STUB_TOML:-}" ] || exit 1
printf '%s\n' "$STUB_TOML" > "$out"
STUBEOF
chmod +x "$STUB"
disk="$WORK/sda"; : > "$disk"

hint() ( # -> DHCP_HOST_ARGS ; console in hint.out
    set +e
    STORM_STORMBLOCK="$STUB"; STORM_RUN="$WORK"; export STUB_TOML
    SLAB="${SLAB_ARG-$disk}"; BOOTTAG="${TAG:-}"; BOOTTAG_FROM="${FROM:-}"
    . "$WORK/hint.sh" > "$WORK/hint.out" 2>&1
    echo "$DHCP_HOST_ARGS"
)

TOML='[node]
hostname = "Stormblock1"   # the reservation
edition = "kubernetes"

[node.labels]
hostname = "not-this"'
check "the declared name, lower-cased" "-x hostname:stormblock1" "$(STUB_TOML="$TOML" TAG=server8 FROM=firmware hint)"
contains "and the console says so" "its declared name" "$(cat "$WORK/hint.out")"
check "no declared name: the firmware's boot name" "-x hostname:server8" "$(TAG=server8.g16.lo FROM=firmware hint)"
check "an SMBIOS guess is not a name" "" "$(TAG=S11075924402016 FROM=smbios-serial hint)"
check "a provisional mac- name is not a name" "" "$(TAG=mac-0cc47a06f96d FROM=firmware hint)"
check "a toml with no [node] hostname: the firmware's" "-x hostname:server8" \
    "$(STUB_TOML='[node.labels]
hostname = "nope"' TAG=server8 FROM=firmware hint)"
check "no disk and no firmware name: nothing sent" "" "$(SLAB_ARG="" hint)"
check "a name with odd characters is made a hostname" "-x hostname:node-1" "$(TAG='Node_1' FROM=firmware hint)"

[ "$fail" = 0 ] && echo "all node name checks passed" || { echo "FAILURES"; exit 1; }
