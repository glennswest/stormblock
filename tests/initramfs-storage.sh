#!/bin/sh
# The install-config's `storage:` in /init (#573; stormcos docs/STORAGE-LAYOUT.md).
#
# Three blocks: the layout (what the engine's `image storage-plan` makes of the
# config and the drives seen) and the format (every drive taken formatted to
# its block size and protection, all at once) and the lay (every drive
# partitioned alike, the data regions one stormraid set). What matters most here is
# what must not happen: a format is every byte on the drive, so a keep-data
# reinstall, an ordinary boot over a drive that holds something, or a plan
# that stops, formats nothing.
#
# Runs the real code: each block is extracted from the init script this repo
# generates, between its marker comments, so the test cannot drift from what
# ships. The engine and stormdrive are stand-ins that record what they were
# asked.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN storage layout/,/# --- END storage layout/p' "$GEN" > "$WORK/layout.sh"
grep -q 'END storage layout' "$WORK/layout.sh" || { echo "FAIL: could not extract the storage layout block"; exit 1; }
sed -n '/# --- BEGIN storage format/,/# --- END storage format/p' "$GEN" > "$WORK/format.sh"
grep -q 'END storage format' "$WORK/format.sh" || { echo "FAIL: could not extract the storage format block"; exit 1; }
# The lay block, its device checks pointed at files under $WORK/dev.
sed -n '/# --- BEGIN storage lay (/,/# --- END storage lay$/p' "$GEN" | sed 's/\[ -b /[ -e /g; s/\[ ! -b /[ ! -e /g' > "$WORK/lay.sh"
grep -q 'END storage lay$' "$WORK/lay.sh" || { echo "FAIL: could not extract the storage lay block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}
has() { # name file pattern
    if grep -q -- "$3" "$2" 2>/dev/null; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: no '$3' in $(basename "$2")"
        sed 's/^/  | /' "$2" 2>/dev/null | tail -20
        fail=1
    fi
}
hasnt() { # name file pattern
    if grep -q -- "$3" "$2" 2>/dev/null; then
        echo "  FAIL  $1: '$3' in $(basename "$2")"
        sed 's/^/  | /' "$2" | tail -20
        fail=1
    else
        echo "  ok    $1"
    fi
}

# Four drives, sda..sdd, as sysfs shows them.
SYS="$WORK/sys"
for d in sda sdb sdc sdd; do
    mkdir -p "$SYS/$d"
    echo 19532873728 > "$SYS/$d/size"
    echo 0 > "$SYS/$d/removable"
done

# The engine: `image storage-plan` answers what plan.out says (exit
# plan.rc) and keeps its arguments; `slab list DEV` prints slabs.<name>.
cat > "$WORK/sb" <<EOF
#!/bin/sh
W="$WORK"
EOF
cat >> "$WORK/sb" <<'EOF'
case "$1 $2" in
"image storage-plan")
    shift 2
    for a in "$@"; do printf '%s\n' "$a"; done > "$W/plan.args"
    cat "$W/plan.out" 2>/dev/null
    exit "$(cat "$W/plan.rc" 2>/dev/null || echo 0)" ;;
"slab list")
    cat "$W/slabs.${3##*/}" 2>/dev/null
    exit 0 ;;
"image storage-lay")
    shift 2
    for a in "$@"; do printf '%s\n' "$a"; done > "$W/lay.args"
    echo "Storage: laid" >&2
    [ -e "$W/lay.rc" ] && { echo "error: laying /dev/sdb: short write" >&2; exit 1; }
    m=""; leg=""
    for a in "$@"; do
        [ -n "$leg" ] && { leg=""; continue; }
        case "$a" in
        --leg|--system-bytes) leg=1 ;;
        /dev/sd*) m="$m $W/dev/${a##*/}3"; touch "$W/dev/${a##*/}3" ;;
        esac
    done
    echo "STORAGE_MEMBERS='${m# }'"
    echo "STORAGE_REGION=9000"; echo "STORAGE_SYSTEM_REGION=800"; echo "STORAGE_BOOT_AREA=100"
    exit 0 ;;
esac
exit 2
EOF
chmod +x "$WORK/sb"

# stormdrive: `--method none` exits 4 for a drive at.<name> names, else 5;
# a format logs its start and end, takes a second, exits rc.<name>.<prot>.
cat > "$WORK/stormdrive" <<EOF
#!/bin/sh
W="$WORK"
EOF
cat >> "$WORK/stormdrive" <<'EOF'
dev=""; prot=""; meth=""
while [ $# -gt 0 ]; do
    case "$1" in
    --device) dev=$2; shift ;;
    --protection) prot=$2; shift ;;
    --method) meth=$2; shift ;;
    esac
    shift
done
n=${dev##*/}
if [ "$meth" = none ]; then
    [ -e "$W/at.$n" ] && exit 4
    exit 5
fi
echo "start $n $prot" >> "$W/format.log"
echo "formatting $n" >&2
sleep 1
echo "end $n $prot" >> "$W/format.log"
exit "$(cat "$W/rc.$n.$prot" 2>/dev/null || echo 0)"
EOF
chmod +x "$WORK/stormdrive"

# stormraid: create keeps its arguments (exit raid.rc); serve keeps its
# and makes the device.
cat > "$WORK/stormraid" <<EOF
#!/bin/sh
W="$WORK"
EOF
cat >> "$WORK/stormraid" <<'EOF'
case "$1" in
create) shift; for a in "$@"; do printf '%s\n' "$a"; done > "$W/raid.args"
    echo "stormraid: created node-data"
    exit "$(cat "$W/raid.rc" 2>/dev/null || echo 0)" ;;
serve) shift; for a in "$@"; do printf '%s\n' "$a"; done > "$W/serve.args"
    touch "$W/dev/ublkb$2"; exit 0 ;;
esac
exit 2
EOF
chmod +x "$WORK/stormraid"

# The /init around the blocks: what they call, stubbed.
cat > "$WORK/env.sh" <<EOF
SURVEY_SB="$WORK/sb"
SURVEY_SYS="$SYS"
STORM_RUN="$WORK/run"
STORM_STORMDRIVE="$WORK/stormdrive"
STORM_STORMRAID="$WORK/stormraid"
STORAGE_DEV="$WORK/dev"
W="$WORK"
EOF
cat >> "$WORK/env.sh" <<'EOF'
mkdir -p "$STORM_RUN" "$STORAGE_DEV"
drive_ident() { echo "ID-${1##*/}"; }
sd_drives() { for d in sda sdb sdc sdd; do echo "$SURVEY_SYS/$d"; done; }
shelf_of() { echo "-|0|0"; }
drive_serial() { echo "SER-${1##*/}"; }
drive_wwn() { echo ""; }
drive_model() { echo "X378"; }
disk_of() { echo "/dev/$(echo "${1##*/}" | sed 's/[0-9]*$//')"; }
drive_signature() { cat "$W/sig.${1##*/}" 2>/dev/null || true; }
sd_list() { echo "  (the drives)"; }
ld_skip() { echo "skip $1" >> "$W/skips"; }
ld_write() { echo "ldwrite $*" >> "$W/ldwrite"; }
rescue_shell() { echo RESCUE; exit 0; }
EOF

reset() {
    rm -rf "$WORK/run" "$WORK"/plan.* "$WORK"/slabs.* "$WORK"/at.* "$WORK"/rc.* \
           "$WORK"/sig.* "$WORK/format.log" "$WORK/skips" "$WORK/ldwrite" "$WORK/out" \
           "$WORK/install-config.yaml" "$WORK/dev" "$WORK"/lay.args "$WORK"/lay.rc \
           "$WORK"/raid.* "$WORK/serve.args"
}

layout() { # vars... -> runs the layout block, prints its variables to out
    {
        cat "$WORK/env.sh"
        for v in "$@"; do echo "$v"; done
        cat "$WORK/layout.sh"
        cat <<'EOF'
echo "=NAMED_DISK=$NAMED_DISK"
echo "=CHOSEN_BY=$CHOSEN_BY"
echo "=STORAGE_DRIVES=$STORAGE_DRIVES"
echo "=STORAGE_FORCE=$STORAGE_FORCE"
echo "=ASSIMILATE=$ASSIMILATE"
echo "=INSTALL_OVER=$INSTALL_OVER"
EOF
    } > "$WORK/run-layout.sh"
    sh "$WORK/run-layout.sh" > "$WORK/out" 2>&1 || echo "EXIT $?" >> "$WORK/out"
}
val() { sed -n "s/^=$1=//p" "$WORK/out"; }

format() { # vars... -> runs the format block
    {
        cat "$WORK/env.sh"
        echo 'STORAGE_FROM=/run/install-config.yaml STORAGE_BLOCK_SIZE=4096 STORAGE_PROTECTION=type1 STORAGE_METHOD=auto'
        echo 'LOCAL_DISK=/dev/sda4 INSTALL_KEEP="" INSTALL_FRESH=1 STORAGE_FORCE="" FORCE_LOCAL=""'
        for v in "$@"; do echo "$v"; done
        cat "$WORK/format.sh"
        cat <<'EOF'
echo "=FORMATTED=$STORAGE_FORMATTED"
echo "=FORCE_LOCAL=$FORCE_LOCAL"
echo "=INSTALL_KEEP=$INSTALL_KEEP"
EOF
    } > "$WORK/run-format.sh"
    sh "$WORK/run-format.sh" > "$WORK/out" 2>&1 || echo "EXIT $?" >> "$WORK/out"
}

lay() { # vars... -> runs the lay block
    {
        cat "$WORK/env.sh"
        echo 'STORAGE_FROM=/run/install-config.yaml STORAGE_SYSTEM_BYTES=0 STORAGE_RAID=yes STORAGE_PARITY=1 STORAGE_SPARES=0'
        echo 'LOCAL_DISK=/dev/sda4 SYSTEM_DISK=/dev/sdb PAIR_DISK=/dev/sdc INSTALL_KEEP="" INSTALL_FRESH=1 STORAGE_FORCE="" FORCE_LOCAL="" STORAGE_FORMATTED=""'
        for v in "$@"; do echo "$v"; done
        echo 'storage_fail() { echo "FATAL: $1 (#573)"; ld_write failed "${LOCAL_DISK:-}" "storage: $1"; rescue_shell; }'
        cat "$WORK/lay.sh"
        cat <<'EOF'
echo "=LOCAL_DISK=$LOCAL_DISK"
echo "=SYSTEM_DISK=$SYSTEM_DISK"
echo "=SYSTEM_DISK_ID=$SYSTEM_DISK_ID"
echo "=PAIR_DISK=$PAIR_DISK"
echo "=FORCE_LOCAL=$FORCE_LOCAL"
echo "=MEMBERS=$STORAGE_MEMBERS"
EOF
    } > "$WORK/run-lay.sh"
    sh "$WORK/run-lay.sh" > "$WORK/out" 2>&1 || echo "EXIT $?" >> "$WORK/out"
}

BASE='OVERRIDE="" ASSIMILATE="" SLAB="" NAMED_DISK="" CHOSEN_BY="" INSTALL_OVER=""'

echo "the layout: no storage: changes nothing"
reset
printf 'hostname: n1\n' > "$WORK/install-config.yaml"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'NAMED_DISK=sdb CHOSEN_BY=named'
check "no storage: keeps the named drive" "sdb" "$(val NAMED_DISK)"
check "no storage: keeps how it was chosen" "named" "$(val CHOSEN_BY)"
check "no storage: never asks the engine" "no" "$([ -e "$WORK/plan.args" ] && echo yes || echo no)"

echo "the layout: a plan takes the drives"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
cat > "$WORK/plan.out" <<'EOF'
STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd'
STORAGE_SYSTEM='/dev/sda /dev/sdb /dev/sdc /dev/sdd'
STORAGE_SAID='STORAGE: 4 drives'
EOF
echo "superblock: data slab" > "$WORK/slabs.sda"
echo "gpt (other)" > "$WORK/sig.sdc"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'INSTALL_OVER=/dev/sda4'
check "the first system drive is the one named" "sda" "$(val NAMED_DISK)"
check "chosen by storage" "storage" "$(val CHOSEN_BY)"
check "all four drives" "/dev/sda /dev/sdb /dev/sdc /dev/sdd" "$(val STORAGE_DRIVES)"
check "the node's own drive stays this node's" "/dev/sda4" "$(val INSTALL_OVER)"
has "it says the plan" "$WORK/out" "  STORAGE: 4 drives"
has "each drive told to the engine" "$WORK/plan.args" "dev=/dev/sdd;serial=SER-sdd;"
has "the config named" "$WORK/plan.args" "install-config.yaml"
has "a drive's bytes" "$WORK/plan.args" "dev=/dev/sdb;.*bytes=10000831348736;"
has "a foreign table said by name" "$WORK/plan.args" "dev=/dev/sdc;.*carries=gpt (other);"
has "this node's own drive carries nothing" "$WORK/plan.args" "dev=/dev/sda;.*carries=;"
hasnt "no wipe word, no --force" "$WORK/plan.args" "^--force$"

echo "the layout: the wipe word reaches the engine"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
printf "STORAGE_DRIVES='/dev/sdb'\nSTORAGE_SYSTEM='/dev/sdb'\n" > "$WORK/plan.out"
echo "superblock: data slab" > "$WORK/slabs.sda"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'ASSIMILATE=force INSTALL_OVER=/dev/sda4'
has "--force passed" "$WORK/plan.args" "^--force$"
check "STORAGE_FORCE kept for the format" "1" "$(val STORAGE_FORCE)"
check "a drive it does not take is no longer this node's" "" "$(val INSTALL_OVER)"
has "and that is said" "$WORK/out" "storage: does not take it"

echo "the layout: a plan that stops takes nothing"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
echo "STORAGE_STOP='/dev/sdc holds another node (n2)'" > "$WORK/plan.out"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'NAMED_DISK=sda'
check "no drive named" "" "$(val NAMED_DISK)"
check "no drives taken" "" "$(val STORAGE_DRIVES)"
check "the install is ambiguous" "ambiguous" "$(val ASSIMILATE)"
has "the STOP says why" "$WORK/out" "STOP: storage: .*/dev/sdc holds another node (n2)"
check "every drive skipped" "4" "$(wc -l < "$WORK/skips" | tr -d ' ')"

echo "the layout: an engine that fails stops it"
reset
printf 'storage:\n  drives: [bogus\n' > "$WORK/install-config.yaml"
echo "error: storage: unclosed [" > "$WORK/plan.out"; echo 1 > "$WORK/plan.rc"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml"
check "nothing taken" "" "$(val STORAGE_DRIVES)"
has "the engine's words said" "$WORK/out" "could not plan storage: (error: storage: unclosed \[)"

echo "the layout: recovery, assimilate=off and a boot from a named slab lay nothing"
for v in 'OVERRIDE=recovery' 'ASSIMILATE=off' 'SLAB=/dev/sdb4'; do
    reset
    printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
    printf "STORAGE_DRIVES='/dev/sda'\n" > "$WORK/plan.out"
    layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" "$v"
    check "$v: nothing taken" "" "$(val STORAGE_DRIVES)"
    check "$v: the engine not asked" "no" "$([ -e "$WORK/plan.args" ] && echo yes || echo no)"
done

echo "the format: every drive at once"
reset
format "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd'"
check "all four formatted" "/dev/sda /dev/sdb /dev/sdc /dev/sdd" "$(val FORMATTED)"
check "four formats" "4" "$(grep -c '^start' "$WORK/format.log")"
check "every format started before the first ended" "start start start start" \
    "$(head -4 "$WORK/format.log" | cut -d' ' -f1 | tr '\n' ' ' | sed 's/ $//')"
check "the install forces the fresh drive" "1" "$(val FORCE_LOCAL)"
has "progress named by drive" "$WORK/out" "  sdc: formatting sdc"
has "the loss said" "$WORK/out" "every byte on them is lost"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the format: a drive at the format already is left"
reset
touch "$WORK/at.sda" "$WORK/at.sdb"
format "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc'"
check "only the other formatted" "/dev/sdc" "$(val FORMATTED)"
has "and said" "$WORK/out" "/dev/sda is at 4096 + PI type1 already"

echo "the format: every drive at the format formats nothing"
reset
touch "$WORK/at.sda" "$WORK/at.sdb"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_KEEP=1 INSTALL_FRESH=""'
check "nothing formatted" "" "$(val FORMATTED)"
check "keep-data stands" "1" "$(val INSTALL_KEEP)"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the format: PI type 1 refused -> none"
reset
echo 3 > "$WORK/rc.sdb.type1"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'"
check "both formatted" "/dev/sda /dev/sdb" "$(val FORMATTED)"
has "sdb again with none" "$WORK/format.log" "start sdb none"
hasnt "sda not again" "$WORK/format.log" "start sda none"
has "said" "$WORK/out" "/dev/sdb refused PI type 1 - formatted with none instead"

echo "the format: a failure is fatal, and nothing is laid"
reset
echo 1 > "$WORK/rc.sdc.type1"
format "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc'"
has "rescue" "$WORK/out" "RESCUE"
has "names the drive" "$WORK/out" "FATAL: the format failed on /dev/sdc(exit 1)"
has "the install result says failed" "$WORK/ldwrite" "ldwrite failed /dev/sda4 storage:"
hasnt "never reaches the end" "$WORK/out" "=FORMATTED="

echo "the format: a keep-data reinstall over this node's data formats nothing"
reset
echo "superblock: data slab" > "$WORK/slabs.sda"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_KEEP=1 INSTALL_FRESH=""'
has "STOP" "$WORK/out" "STOP: storage: .* formats /dev/sda /dev/sdb"
has "names what is held" "$WORK/out" "/dev/sda(stormblock slabs)"
has "rescue" "$WORK/out" "RESCUE"
check "no format ran" "no" "$([ -e "$WORK/format.log" ] && echo yes || echo no)"

echo "the format: an ordinary boot over a foreign table formats nothing"
reset
echo "gpt (other)" > "$WORK/sig.sdb"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_FRESH=""'
has "names it" "$WORK/out" "/dev/sdb(gpt (other))"
check "no format ran" "no" "$([ -e "$WORK/format.log" ] && echo yes || echo no)"

echo "the format: an ordinary boot over blank drives formats them"
reset
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_FRESH=""'
check "both formatted" "/dev/sda /dev/sdb" "$(val FORMATTED)"

echo "the format: the wipe word over this node's data goes ahead"
reset
echo "superblock: data slab" > "$WORK/slabs.sda"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_KEEP=1 INSTALL_FRESH="" STORAGE_FORCE=1'
check "both formatted" "/dev/sda /dev/sdb" "$(val FORMATTED)"
check "nothing kept now" "" "$(val INSTALL_KEEP)"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the format: no stormdrive in the initramfs is fatal"
reset
format "STORAGE_DRIVES='/dev/sda'" "STORM_STORMDRIVE=$WORK/none"
has "rescue" "$WORK/out" "RESCUE"
has "names the build setting" "$WORK/out" "STORMDRIVE_BIN (stormcentral#1335)"

echo "the format: no storage: does nothing"
reset
format "STORAGE_DRIVES=''" "STORM_STORMDRIVE=$WORK/none"
check "nothing formatted" "" "$(val FORMATTED)"
hasnt "no rescue" "$WORK/out" "RESCUE"

D4="STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd' STORAGE_SYSTEM='/dev/sda /dev/sdb /dev/sdc /dev/sdd'"

echo "the lay: four drives, one set at parity 1"
reset
lay "$D4"
has "every drive laid" "$WORK/lay.args" "^/dev/sdd$"
check "every drive a leg" "4" "$(grep -c '^--leg$' "$WORK/lay.args")"
has "the system size passed" "$WORK/lay.args" "^--system-bytes$"
has "the set made at parity 1" "$WORK/raid.args" "^--parity$"
has "as node-data" "$WORK/raid.args" "^node-data$"
has "over the old bytes" "$WORK/raid.args" "^--force$"
check "four members" "4" "$(grep -c "^$WORK/dev/sd.3$" "$WORK/raid.args")"
hasnt "no spare" "$WORK/raid.args" "^--spare$"
has "served at id 64" "$WORK/serve.args" "^64$"
check "the data half on the set" "$WORK/dev/ublkb64" "$(val LOCAL_DISK)"
check "the system half on the first leg" "/dev/sda" "$(val SYSTEM_DISK)"
check "known by its identity" "ID-sda" "$(val SYSTEM_DISK_ID)"
check "no pair" "" "$(val PAIR_DISK)"
check "laid fresh" "1" "$(val FORCE_LOCAL)"
has "the regions said" "$WORK/out" "a 9000-byte data region"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the lay: a spare is the last member"
reset
lay "$D4" 'STORAGE_SPARES=1'
check "the spare" "$WORK/dev/sdd3" "$(sed -n '/^--spare$/{n;p;}' "$WORK/raid.args")"
has "said" "$WORK/out" "parity 1, 1 spare(s)"

echo "the lay: data_raid none, or one drive, makes no set"
reset
lay "$D4" 'STORAGE_RAID=no'
check "no set" "no" "$([ -e "$WORK/raid.args" ] && echo yes || echo no)"
check "the first data region" "$WORK/dev/sda3" "$(val LOCAL_DISK)"
has "said" "$WORK/out" "data regions are left laid and empty"
reset
lay "STORAGE_DRIVES='/dev/sdb' STORAGE_SYSTEM='/dev/sdb'"
check "one drive: no set" "no" "$([ -e "$WORK/raid.args" ] && echo yes || echo no)"
check "one drive: its data region" "$WORK/dev/sdb3" "$(val LOCAL_DISK)"

echo "the lay: legs only where system_mirror puts them"
reset
lay "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc' STORAGE_SYSTEM='/dev/sdb /dev/sdc'"
check "two legs" "2" "$(grep -c '^--leg$' "$WORK/lay.args")"
check "the first leg takes the system half" "/dev/sdb" "$(val SYSTEM_DISK)"

echo "the lay: a keep-data reinstall over a node's drives lays nothing"
reset
echo "superblock: data slab" > "$WORK/slabs.sdc"
lay "$D4" 'INSTALL_KEEP=1 INSTALL_FRESH=""'
has "STOP" "$WORK/out" "STOP: storage: .* lays /dev/sda"
has "names it" "$WORK/out" "/dev/sdc(stormblock slabs)"
has "rescue" "$WORK/out" "RESCUE"
check "nothing laid" "no" "$([ -e "$WORK/lay.args" ] && echo yes || echo no)"

echo "the lay: an ordinary boot over a foreign table lays nothing"
reset
echo "gpt (other)" > "$WORK/sig.sdb"
lay "$D4" 'INSTALL_FRESH=""'
has "names it" "$WORK/out" "/dev/sdb(gpt (other))"
check "nothing laid" "no" "$([ -e "$WORK/lay.args" ] && echo yes || echo no)"

echo "the lay: drives this boot formatted are blank"
reset
echo "superblock: data slab" > "$WORK/slabs.sdc"
lay "$D4" 'INSTALL_KEEP=1 INSTALL_FRESH="" STORAGE_FORMATTED="/dev/sdc"'
has "laid" "$WORK/lay.args" "^/dev/sdc$"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the lay: the wipe word lays over a node's data"
reset
echo "superblock: data slab" > "$WORK/slabs.sda"
lay "$D4" 'INSTALL_KEEP=1 INSTALL_FRESH="" STORAGE_FORCE=1'
has "laid" "$WORK/lay.args" "^/dev/sda$"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the lay: a lay or a create that fails is fatal"
reset
touch "$WORK/lay.rc"
lay "$D4"
has "the engine's words" "$WORK/out" "FATAL: laying the drives failed: error: laying /dev/sdb: short write"
check "no set" "no" "$([ -e "$WORK/raid.args" ] && echo yes || echo no)"
reset
echo 1 > "$WORK/raid.rc"
lay "$D4"
has "create failed" "$WORK/out" "FATAL: stormraid create failed: stormraid: created node-data"
check "not served" "no" "$([ -e "$WORK/serve.args" ] && echo yes || echo no)"

echo "the lay: no stormraid in the initramfs is fatal"
reset
lay "$D4" "STORM_STORMRAID=$WORK/none"
has "names the build" "$WORK/out" "stormcentral#1335"
check "not created" "no" "$([ -e "$WORK/raid.args" ] && echo yes || echo no)"

echo "the lay: no storage: lays nothing"
reset
lay "STORAGE_DRIVES=''"
check "nothing laid" "no" "$([ -e "$WORK/lay.args" ] && echo yes || echo no)"
check "the local disk stands" "/dev/sda4" "$(val LOCAL_DISK)"
check "the system disk stands" "/dev/sdb" "$(val SYSTEM_DISK)"

if [ "$fail" = 0 ]; then
    echo "initramfs-storage: all ok"
else
    echo "initramfs-storage: FAILED"
    exit 1
fi
