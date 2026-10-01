#!/bin/sh
# The network half of a split NIC driver, and the wait for late netdevs (#250).
#
# The X9 blades' ConnectX-3 matches mlx4_core by PCI ID; its Ethernet ports
# are mlx4_en, matched only by an auxiliary device mlx4_core makes at the end
# of a slow probe - after the modalias walk had stopped. The initramfs then
# counted the Intel port alone. This pins both halves of the fix against a
# fake /sys: the half is asked for by name once its core is loaded, and the
# uplink selection waits until every bound network function has a netdev.
#
# Runs the real code: the blocks are extracted from the init script this repo
# generates, between their marker comments, so the test cannot drift.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN protocol halves/,/# --- END protocol halves/p' "$GEN" > "$WORK/halves.sh"
[ -s "$WORK/halves.sh" ] || { echo "FAIL: could not extract the protocol halves block"; exit 1; }
sed -n '/# --- BEGIN netdev wait/,/# --- END netdev wait/p' "$GEN" > "$WORK/wait.sh"
[ -s "$WORK/wait.sh" ] || { echo "FAIL: could not extract the netdev wait block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

# A modprobe that records what it was asked for, "loads" it into the fake
# /sys/module, and - for mlx4_en, or the aux alias that names it - makes the
# netdev of every mlx4 function appear (what the real driver does when it
# binds). STUB_FAIL names a module that will not load.
STUB="$WORK/modprobe"
cat > "$STUB" <<'STUBEOF'
#!/bin/sh
[ "$1" = -q ] && shift
echo "$1" >> "$STUB_LOG"
case "$1" in auxiliary:mlx4_core.eth) m=mlx4_en ;; *) m="$1" ;; esac
[ "$m" = "${STUB_FAIL:-}" ] && exit 1
mkdir -p "$STUB_MODULE/$m"
if [ "$m" = mlx4_en ] && [ -z "${STUB_NO_NETDEV:-}" ]; then
    for d in "$STUB_PCI"/*; do
        [ "$(cat "$d/drv")" = mlx4_core ] && mkdir -p "$d/net/eth1"
    done
fi
exit 0
STUBEOF
chmod +x "$STUB"

# pci-addr:class:driver[:netdev] -> a fake /sys/bus/pci/devices and /sys/module
make_sys() { # name spec...
    root="$WORK/sys.$1"; shift
    rm -rf "$root"; mkdir -p "$root/pci" "$root/module" "$root/aux"
    for spec in "$@"; do
        a=${spec%%:*}; rest=${spec#*:}
        c=${rest%%:*}; rest=${rest#*:}
        drv=${rest%%:*}; nd=""; [ "$rest" != "$drv" ] && nd=${rest#*:}
        d="$root/pci/$a"; mkdir -p "$d"
        echo "$c" > "$d/class"; echo "$drv" > "$d/drv"
        echo "pci:v0000XXXX" > "$d/modalias"
        if [ "$drv" != - ]; then
            mkdir -p "$root/drivers/$drv" "$root/module/$drv"
            ln -s "$root/drivers/$drv" "$d/driver"
        fi
        [ -n "$nd" ] && mkdir -p "$d/net/$nd"
    done
    echo "$root"
}

# -> "<what modprobe was asked, space-separated>|<interfaces of every pci fn>|<wait output's last line>"
run() { # root [wait seconds]
    (
        set +e
        root="$1"
        STUB_LOG="$root/log"; : > "$STUB_LOG"
        STUB_MODULE="$root/module"; STUB_PCI="$root/pci"
        export STUB_LOG STUB_MODULE STUB_PCI
        STORM_MODPROBE="$STUB"; STORM_SYS_MODULE="$root/module"
        STORM_PCI_SYSFS="$root/pci"; STORM_AUX_SYSFS="$root/aux"
        STORM_NETDEV_WAIT="${2:-3}"
        sleep() { :; }
        . "$WORK/halves.sh" > "$root/out" 2>&1
        . "$WORK/wait.sh" >> "$root/out" 2>&1
        nets=$(for d in "$root"/pci/*; do ls "$d/net" 2>/dev/null; done | tr '\n' ' ')
        echo "$(tr '\n' ' ' < "$STUB_LOG" | sed 's/ $//')|${nets% }|$(tail -n 1 "$root/out")"
    )
}

echo "protocol halves and netdev wait:"

# An X9 blade: ixgbe has its port, mlx4_core bound the ConnectX-3 and no
# netdev exists yet.
t=$(make_sys x9 0000:01:00.0:0x020000:ixgbe:eth0 0000:05:00.0:0x020000:mlx4_core)
check "mlx4_core loaded: mlx4_en is asked for and its port appears" \
    "mlx4_en|eth0 eth1|  mlx4_en loaded: the network half of mlx4_core" \
    "$(run "$t" 0)"

# Already loaded: not asked for again, nothing to wait for.
t=$(make_sys x9b 0000:01:00.0:0x020000:ixgbe:eth0 0000:05:00.0:0x020000:mlx4_core:eth1)
mkdir -p "$t/module/mlx4_en"
check "mlx4_en already loaded: nothing asked, nothing waited" "|eth0 eth1|" "$(run "$t")"

# No Mellanox: nothing extra is loaded.
t=$(make_sys intel 0000:01:00.0:0x020000:ixgbe:eth0)
check "no mlx4_core: nothing extra" "|eth0|" "$(run "$t")"

# mlx4_en refuses to load: said so, then the wait names the function.
t=$(make_sys broken 0000:01:00.0:0x020000:ixgbe:eth0 0000:05:00.0:0x020000:mlx4_core)
out=$(STUB_FAIL=mlx4_en run "$t" 2)
check "a half that will not load is named, and the wait gives up bounded" \
    "    0000:05:00.0 driver mlx4_core pci:v0000XXXX" "${out##*|}"
check "and says it" "1" "$(grep -c 'mlx4_en would not load' "$t/out")"

# The half loaded but its device came late: the wait walks the aux bus and
# the alias brings the port.
t=$(make_sys late 0000:01:00.0:0x020000:ixgbe:eth0 0000:05:00.0:0x020000:mlx4_core)
mkdir -p "$t/aux/mlx4_core.eth.0"; echo "auxiliary:mlx4_core.eth" > "$t/aux/mlx4_core.eth.0/modalias"
out=$(STUB_NO_NETDEV=1 run "$t" 2)
check "a port that never appears is waited for, bounded, and named" \
    "0000:05:00.0" "$(grep -o '0000:05:00.0' "$t/out" | head -1)"
check "the aux bus is walked during the wait" "1" \
    "$(grep -c 'auxiliary:mlx4_core.eth' "$t/log")"

# Functions that are not NICs, or that no driver took, are not waited for.
t=$(make_sys other 0000:00:1f.2:0x010601:ahci 0000:06:00.0:0x020000:-)
check "a non-network or driverless function is not waited for" "||" "$(run "$t")"

[ "$fail" -eq 0 ] && echo "all protocol-half and netdev-wait checks passed"
exit "$fail"
