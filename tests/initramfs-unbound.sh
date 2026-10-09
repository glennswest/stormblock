#!/bin/sh
# Storage and network controllers left with no driver (#89), against a fake
# sysfs.
#
# A Dell with a SAS3008 and a BCM5720 booted with mpt3sas and tg3 in the
# image, their dependencies met, and neither bound: no disks behind the HBA,
# no onboard ports, and not a word. Pinned here:
#   * a storage or network function with no driver gets its modalias's
#     module loaded and a re-probe; one that binds then is said to have;
#   * one still unbound is a WARNING with the reason: no module matches, a
#     module matches and did not load, or a loaded module did not take it
#     (with the kernel's last word on the device);
#   * other classes, and bound functions, are not mentioned; nothing unbound
#     is nothing said.
#
# Runs the real code: the block is extracted from the init script this repo
# generates, between its marker comments, so the test cannot drift.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN unbound controllers/,/# --- END unbound controllers/p' "$GEN" > "$WORK/ub.sh"
[ -s "$WORK/ub.sh" ] || { echo "FAIL: could not extract the unbound controllers block"; exit 1; }

fail=0
contains() { # name needle haystack
    case "$3" in
    *"$2"*) echo "  ok    $1" ;;
    *) echo "  FAIL  $1: '$2' not in: $3"; fail=1 ;;
    esac
}
lacks() { # name needle haystack
    case "$3" in
    *"$2"*) echo "  FAIL  $1: '$2' in: $3"; fail=1 ;;
    *) echo "  ok    $1" ;;
    esac
}

# modprobe: `-q <alias>` loads what the alias names (and, for the HBA's,
# binds it, as a driver that came late does on a re-probe); `-R <alias>`
# names the modules that match.
STUB="$WORK/modprobe"
cat > "$STUB" <<'STUBEOF'
#!/bin/sh
case "$1" in
-R)
    case "$2" in
    *sas3008*) echo mpt3sas ;;
    *bcm5720*) echo tg3 ;;
    *megaraid*) echo megaraid_sas ;;
    esac
    exit 0 ;;
-q) shift ;;
esac
case "$1" in
*sas3008*)
    mkdir -p "$STUB_SYS/module/mpt3sas" "$STUB_SYS/drivers/mpt3sas"
    ln -s "$STUB_SYS/drivers/mpt3sas" "$STUB_SYS/pci/0000:01:00.0/driver" ;;
*bcm5720*) mkdir -p "$STUB_SYS/module/tg3" ;;
esac
exit 0
STUBEOF
chmod +x "$STUB"

# pci/class/vendor:device/alias/driver ('-' = none)
make_sys() {
    root="$WORK/sys.$1"; shift
    rm -rf "$root"; mkdir -p "$root/pci" "$root/module" "$root/drivers"
    for spec in "$@"; do
        a=${spec%%/*}; rest=${spec#*/}
        c=${rest%%/*}; rest=${rest#*/}
        id=${rest%%/*}; rest=${rest#*/}
        al=${rest%%/*}; drv=${rest#*/}
        d="$root/pci/$a"; mkdir -p "$d"
        echo "$c" > "$d/class"; echo "0x${id%%:*}" > "$d/vendor"; echo "0x${id#*:}" > "$d/device"
        echo "$al" > "$d/modalias"
        if [ "$drv" != - ]; then
            mkdir -p "$root/drivers/$drv"
            ln -s "$root/drivers/$drv" "$d/driver"
        fi
    done
    echo "$root"
}

run() { # root -> the block's output
    (
        set +e
        root="$1"
        STUB_SYS="$root"; export STUB_SYS
        MODPROBE="$STUB"
        STORM_PCI_SYSFS="$root/pci"; STORM_PCI_PROBE="$root/probe"
        STORM_MODULE_SYSFS="$root/module"; STORM_UNBOUND_WAIT=1
        sleep() { :; }
        dmesg() { echo "[2.1] tg3 0000:03:00.0: Problem fetching invariants of chip, aborting"; }
        . "$WORK/ub.sh" 2>&1
    )
}

echo "unbound controllers:"

# The Dell: the HBA binds on a second probe; the BCM5720's module is loaded
# and did not take it; an unknown NIC no module matches; a RAID card whose
# module matched and did not load; a VGA function and the bound ConnectX-4,
# neither mentioned.
t=$(make_sys dell \
    0000:01:00.0/0x010700/1000:0097/pci:sas3008/- \
    0000:03:00.0/0x020000/14e4:165f/pci:bcm5720/- \
    0000:04:00.0/0x020000/abcd:0001/pci:unknown/- \
    0000:05:00.0/0x010400/1000:005d/pci:megaraid/- \
    0000:06:00.0/0x030000/102b:0536/pci:vga/- \
    0000:02:00.0/0x020000/15b3:1015/pci:cx4/mlx5_core)
out=$(run "$t")
contains "the HBA binds on a second probe" "storage controller 0000:01:00.0 [1000:0097]: bound to mpt3sas on a second probe" "$out"
contains "a loaded module that did not take it is named" "WARNING: network controller 0000:03:00.0 [14e4:165f] (class 0x020000) has no driver: tg3 is loaded and did not take it" "$out"
contains "with the kernel's last word on it" "Problem fetching invariants" "$out"
contains "no module matches: said" "WARNING: network controller 0000:04:00.0 [abcd:0001] (class 0x020000) has no driver: no module in this image matches it" "$out"
contains "a module that matched and did not load: said" "WARNING: storage controller 0000:05:00.0 [1000:005d] (class 0x010400) has no driver: megaraid_sas would match and did not load" "$out"
lacks "a display function is not mentioned" "0000:06:00.0" "$out"
lacks "a bound function is not mentioned" "0000:02:00.0" "$out"
# drivers_probe is written one address at a time; the fake keeps the last.
contains "unbound ones are re-probed" "0000:05:00.0" "$(cat "$t/probe" 2>/dev/null; echo)"

# Nothing unbound: nothing said.
t=$(make_sys clean 0000:02:00.0/0x020000/15b3:1015/pci:cx4/mlx5_core 0000:01:00.0/0x010700/1000:0097/pci:sas3008/mpt3sas)
out=$(run "$t")
if [ -z "$out" ]; then echo "  ok    everything bound: nothing said"; else echo "  FAIL  everything bound, yet: $out"; fail=1; fi

[ "$fail" -eq 0 ] && echo "ALL PASS" || { echo "FAILED"; exit 1; }
