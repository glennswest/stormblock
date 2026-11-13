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
[ "$1" = serve ] || exit 2
shift
for a in "$@"; do printf '%s\n' "$a"; done > "$W/serve.args"
cat "$W/serve.say" 2>/dev/null
[ -e "$W/serve.dead" ] && { echo "stormraid: /dev/sdb3 belongs to another set"; exit 1; }
touch "$W/dev/ublkb$2"
sleep 30
EOF
chmod +x "$WORK/stormraid"

# stormblock: `slab partner` keeps its arguments.
cat > "$WORK/stormblock" <<EOF
#!/bin/sh
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
    rm -rf "$WORK/sys" "$WORK/dev" "$WORK/run" "$WORK"/serve.* "$WORK/partner.args" "$WORK/out"
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
storage_unserve >/dev/null
echo "=AFTER=$STORAGE_SERVED"
EOF
    } > "$WORK/run.sh"
    sh "$WORK/run.sh" > "$WORK/out" 2>&1 || echo "EXIT $?" >> "$WORK/out"
}
val() { sed -n "s/^=$1=//p" "$WORK/out"; }

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

echo "a member missing: served, and said loudly"
reset
for d in sda sdb sdc; do member "$d" "${d}3" stormblock-raid STORMRD1; done
echo "stormraid: slot 3 (abcd) is missing" > "$WORK/serve.say"
run
check "served" "$WORK/dev/ublkb64" "$(val SERVED)"
has "DEGRADED" "$WORK/out" "NODE-DATA SET DEGRADED"
has "names the slot" "$WORK/out" "slot 3 (abcd) is missing"

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

echo "a set that does not open: said, the boot goes on without it"
reset
member sda sda3 stormblock-raid STORMRD1
touch "$WORK/serve.dead"
run
check "nothing served" "" "$(val SERVED)"
has "said" "$WORK/out" "did not come up"
has "the reason" "$WORK/out" "belongs to another set"
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
