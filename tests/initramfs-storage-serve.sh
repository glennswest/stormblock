#!/bin/sh
# The node-data set served on every boot (#573): a node laid by storage:
# keeps its data half on a stormraid set over every drive's `stormblock-raid`
# partition, and /init serves it before the probe looks for the node's
# halves, so the system half on a leg finds its data half on /dev/ublkb64.
# A member missing still boots (degraded, said loudly); a set that does not
# open is said and the boot goes on without it.
#
# Runs the real block, extracted between its markers from the init script
# this repo generates; sysfs, /dev and stormraid are stand-ins.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Device checks pointed at files under $WORK/dev.
sed -n '/# --- BEGIN storage serve/,/# --- END storage serve/p' "$GEN" \
    | sed 's/\[ -b /[ -e /g; s/\[ ! -b /[ ! -e /g' > "$WORK/block.sh"
grep -q 'END storage serve' "$WORK/block.sh" || { echo "FAIL: could not extract the storage serve block"; exit 1; }
sed -n '/# --- BEGIN node halves/,/# --- END node halves/p' "$GEN" > "$WORK/halves.sh"
grep -q 'END node halves' "$WORK/halves.sh" || { echo "FAIL: could not extract the node halves block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then echo "  ok    $1"; else echo "  FAIL  $1: expected '$2', got '$3'"; fail=1; fi
}
has() { # name file pattern
    if grep -q -- "$3" "$2" 2>/dev/null; then echo "  ok    $1"; else echo "  FAIL  $1: no '$3' in $2"; sed 's/^/        /' "$2" 2>/dev/null; fail=1; fi
}
hasnt() { # name file pattern
    if grep -q -- "$3" "$2" 2>/dev/null; then echo "  FAIL  $1: '$3' in $2"; fail=1; else echo "  ok    $1"; fi
}

# stormraid: serve keeps its arguments, says what serve.say holds, and
# makes the device unless serve.dead.
cat > "$WORK/stormraid" <<EOF
#!/bin/sh
W="$WORK"
EOF
cat >> "$WORK/stormraid" <<'EOF'
if [ "$1" = status ]; then
    shift
    for a in "$@"; do echo "$a: set 1111 drive 2222 generation 3"; done
    echo "set 1111: $# slots, parity 1, chunk 256 KiB, 100 stripes, 1000 bytes usable, journal 64 MiB/drive"
    i=0; for a in "$@"; do echo "  $i        active     u$i $a"; i=$((i + 1)); done
    [ -e "$W/status.spare" ] && echo "  spare    active     us /dev/sde3"
    exit 0
fi
[ "$1" = serve ] || exit 2
shift
for a in "$@"; do printf '%s\n' "$a"; done > "$W/serve.args"
cat "$W/serve.say" 2>/dev/null
[ -e "$W/serve.dead" ] && { echo "stormraid: /dev/sdb3 belongs to another set"; exit 1; }
touch "$W/dev/ublkb$2"
sleep 30
EOF
chmod +x "$WORK/stormraid"

# stormblock: `slab list D` says role=system for a drive in system.drives;
# `slab partner` keeps its arguments.
cat > "$WORK/stormblock" <<EOF
#!/bin/sh
if [ "\$2" = list ]; then
    grep -qx "\$3" "$WORK/system.drives" 2>/dev/null && echo "slab x role=system"
    exit 0
fi
printf '%s\n' "\$@" > "$WORK/partner.args"
echo "/dev/ublkb64"
EOF
chmod +x "$WORK/stormblock"

member() { # disk part partname magic|-
    mkdir -p "$WORK/sys/$1/$2"
    printf 'MAJOR=8\nDEVNAME=%s\nPARTNAME=%s\n' "$2" "$3" > "$WORK/sys/$1/$2/uevent"
    if [ "$4" = - ]; then
        dd if=/dev/zero of="$WORK/dev/$2" bs=4096 count=2 2>/dev/null
    else
        { printf '%s' "$4"; dd if=/dev/zero bs=4088 count=1 2>/dev/null; dd if=/dev/zero bs=4096 count=1 2>/dev/null; } > "$WORK/dev/$2"
    fi
}
reset() {
    rm -rf "$WORK/sys" "$WORK/dev" "$WORK/run" "$WORK"/serve.* "$WORK"/status.* "$WORK/partner.args" "$WORK/out" "$WORK/system.drives" "$WORK/layout.json"
    mkdir -p "$WORK/sys" "$WORK/dev"
}
run() { # vars...
    {
        echo "STORM_SYS_BLOCK=$WORK/sys STORAGE_DEV=$WORK/dev STORM_RUN=$WORK/run STORAGE_SERVE_WAIT=5"
        echo "STORM_STORMRAID=$WORK/stormraid STORM_STORMBLOCK=$WORK/stormblock"
        for v in "$@"; do echo "$v"; done
        cat "$WORK/block.sh" "$WORK/halves.sh"
        cat <<'EOF'
storage_serve
echo "=SERVED=$STORAGE_SERVED"
[ -n "$STORAGE_SERVED" ] && partner_of /dev/sda >/dev/null
system_legs
echo "=LEGS=$LEG_ARGS"
storage_report
cp "$STORM_RUN/storage-layout.json" "$STORM_RUN/../layout.json" 2>/dev/null || true
storage_unserve >/dev/null
echo "=AFTER=$STORAGE_SERVED"
EOF
    } > "$WORK/run.sh"
    sh "$WORK/run.sh" > "$WORK/out" 2>&1 || echo "EXIT $?" >> "$WORK/out"
}
val() { sed -n "s/^=$1=//p" "$WORK/out"; }
lay() { # python expression over the report as L -> its value
    python3 -c "import json,sys; L=json.load(open(sys.argv[1])); print($1)" "$WORK/layout.json" 2>&1
}

echo "every drive's member: served at id 64, the partner looked for on it"
reset
for d in sda sdb sdc sdd; do member "$d" "${d}3" stormblock-raid STORMRD1; member "$d" "${d}2" stormblock-system -; done
run
check "served" "$WORK/dev/ublkb64" "$(val SERVED)"
has "at id 64" "$WORK/serve.args" "^64$"
check "four members" "4" "$(grep -c "^$WORK/dev/sd.3$" "$WORK/serve.args")"
hasnt "no system region" "$WORK/serve.args" "sda2"
has "the set named to partner" "$WORK/partner.args" "^--also$"
has "as the served device" "$WORK/partner.args" "^$WORK/dev/ublkb64$"
hasnt "not degraded" "$WORK/out" "DEGRADED"
check "let go of" "" "$(val AFTER)"

echo "the other system legs: every other member drive with a system slab"
reset
for d in sda sdb sdc sdd; do member "$d" "${d}3" stormblock-raid STORMRD1; done
for d in sda sdb sdc; do echo "$WORK/dev/$d"; done > "$WORK/system.drives"
run "SLAB=$WORK/dev/sda"
check "legs" "--system-leg $WORK/dev/sdb --system-leg $WORK/dev/sdc" "$(val LEGS)"
has "said" "$WORK/out" "the other system legs"

echo "the layout reported on a boot (#573 step 8): drives, legs, the set's state"
check "from" "boot" "$(lay 'L["from"]')"
check "drives" "4" "$(lay 'len(L["drives"])')"
check "no format on a later boot" "None" "$(lay 'L["drives"][0]["format"]')"
check "legs" "['$WORK/dev/sda', '$WORK/dev/sdb', '$WORK/dev/sdc']" "$(lay 'L["system_legs"]')"
check "set device" "$WORK/dev/ublkb64" "$(lay 'L["set"]["device"]')"
check "set state" "optimal" "$(lay 'L["set"]["state"]')"
check "parity" "1" "$(lay 'L["set"]["parity"]')"
check "slots" "4" "$(lay 'L["set"]["slots"]')"
check "spares" "0" "$(lay 'L["set"]["spares"]')"
check "members" "4" "$(lay 'len(L["set"]["members"])')"
check "none missing" "[]" "$(lay 'L["set"]["missing"]')"

echo "the layout reported on a kept reinstall says so"
run "SLAB=$WORK/dev/sda" "STORAGE_KEPT=1"
check "from" "reinstall" "$(lay 'L["from"]')"
check "the set's drives" "4" "$(lay 'len(L["drives"])')"

echo "the layout reported on the boot that laid the drives: the format each got"
reset
for d in sda sdb sdc sdd; do member "$d" "${d}3" stormblock-raid STORMRD1; done
mkdir -p "$WORK/run/storage-format"
echo "formatted none auto" > "$WORK/run/storage-format/sda.fmt"
echo "already type1" > "$WORK/run/storage-format/sdb.fmt"
touch "$WORK/status.spare"
run "SLAB=http://forge/x" "SYSTEM_DISK=/dev/sda" "STORAGE_DRIVES='/dev/sda /dev/sdb'" \
    "STORAGE_MEMBERS='/dev/sda3 /dev/sdb3'" "STORAGE_SET=/dev/ublkb64" "STORAGE_SYSTEM='/dev/sda /dev/sdb'" \
    "STORAGE_BLOCK_SIZE=4096" "STORAGE_PROTECTION=type1" "STORAGE_METHOD=auto" "STORAGE_PARITY=1" \
    "STORAGE_BOOT_AREA=1073741824" "STORAGE_SYSTEM_REGION=34359738368" "STORAGE_REGION=9000000000000"
check "from" "install" "$(lay 'L["from"]')"
check "formatted" "formatted none auto" "$(lay '" ".join(L["drives"][0][k] for k in ("format","protection","method"))')"
check "already" "already type1 None" "$(lay '" ".join(str(L["drives"][1][k]) for k in ("format","protection","method"))')"
check "asked" "4096 type1 auto" "$(lay '" ".join(str(L["asked"][k]) for k in ("block_size","protection","method"))')"
check "regions" "34359738368" "$(lay 'L["regions"]["system"]')"
check "legs" "['/dev/sda', '/dev/sdb']" "$(lay 'L["system_legs"]')"
check "set" "/dev/ublkb64 optimal 1" "$(lay '" ".join(str(L["set"][k]) for k in ("device","state","parity"))')"

echo "an install's laid legs: the rest of the system list, the first left out"
reset
member sda sda3 stormblock-raid STORMRD1
run "SLAB=http://forge/x" "SYSTEM_DISK=/dev/sda" "STORAGE_LAID_LEGS='/dev/sda /dev/sdb /dev/sdc'"
check "legs" "--system-leg /dev/sdb --system-leg /dev/sdc" "$(val LEGS)"

echo "no storage: layout: no legs"
reset
member sda sda1 EFI -
run "SLAB=$WORK/dev/sda"
check "none" "" "$(val LEGS)"

echo "a member missing: served, and said loudly"
reset
for d in sda sdb sdc; do member "$d" "${d}3" stormblock-raid STORMRD1; done
echo "stormraid: slot 3 (abcd) is missing" > "$WORK/serve.say"
run
check "served" "$WORK/dev/ublkb64" "$(val SERVED)"
has "DEGRADED" "$WORK/out" "NODE-DATA SET DEGRADED"
has "names the slot" "$WORK/out" "slot 3 (abcd) is missing"
check "reported degraded" "degraded" "$(lay 'L["set"]["state"]')"
check "the missing slot reported" "['slot 3 (abcd) is missing']" "$(lay 'L["set"]["missing"]')"

echo "a partition named so with no superblock is left out"
reset
member sda sda3 stormblock-raid STORMRD1
member sdb sdb3 stormblock-raid -
run
hasnt "left out" "$WORK/serve.args" "sdb3"
has "said" "$WORK/out" "sdb3 is named stormblock-raid and carries no stormraid superblock"

echo "a superblock in block 1 counts"
reset
member sda sda3 stormblock-raid -
{ dd if=/dev/zero bs=4096 count=1 2>/dev/null; printf 'STORMRD1'; dd if=/dev/zero bs=4088 count=1 2>/dev/null; } > "$WORK/dev/sda3"
run
has "taken" "$WORK/serve.args" "sda3"

echo "no member: nothing served, partner asked as before"
reset
member sda sda1 EFI -
run
check "nothing served" "" "$(val SERVED)"
check "stormraid not run" "no" "$([ -e "$WORK/serve.args" ] && echo yes || echo no)"
check "no layout reported" "no" "$([ -e "$WORK/layout.json" ] && echo yes || echo no)"

echo "a set that does not open: said, the boot goes on without it"
reset
member sda sda3 stormblock-raid STORMRD1
touch "$WORK/serve.dead"
run
check "nothing served" "" "$(val SERVED)"
has "said" "$WORK/out" "did not come up"
has "the reason" "$WORK/out" "belongs to another set"
check "reported not served" "None not-served" "$(lay 'str(L["set"]["device"]) + " " + L["set"]["state"]')"
hasnt "no exit" "$WORK/out" "^EXIT"

echo "no stormraid in the initramfs: said, nothing served"
reset
member sda sda3 stormblock-raid STORMRD1
run "STORM_STORMRAID=$WORK/none"
check "nothing served" "" "$(val SERVED)"
has "names the build" "$WORK/out" "stormcentral#1335"

if [ "$fail" = 0 ]; then
    echo "PASS: initramfs storage serve"
else
    echo "FAIL: initramfs storage serve"
    exit 1
fi
