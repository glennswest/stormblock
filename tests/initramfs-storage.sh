#!/bin/sh
# The install-config's `storage:` in /init (#573; stormcos docs/STORAGE-LAYOUT.md).
#
# Two blocks: the layout (what the engine's `image storage-plan` makes of the
# config and the drives seen) and the format (every drive taken formatted to
# its block size and protection, all at once). What matters most here is
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

# The /init around the blocks: what they call, stubbed.
cat > "$WORK/env.sh" <<EOF
SURVEY_SB="$WORK/sb"
SURVEY_SYS="$SYS"
STORM_RUN="$WORK/run"
STORM_STORMDRIVE="$WORK/stormdrive"
W="$WORK"
EOF
cat >> "$WORK/env.sh" <<'EOF'
mkdir -p "$STORM_RUN"
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
           "$WORK/install-config.yaml"
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

if [ "$fail" = 0 ]; then
    echo "initramfs-storage: all ok"
else
    echo "initramfs-storage: FAILED"
    exit 1
fi
