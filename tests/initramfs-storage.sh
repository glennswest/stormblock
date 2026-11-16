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

sed -n '/# --- BEGIN storage format tools/,/# --- END storage format tools/p' "$GEN" > "$WORK/tools.sh"
grep -q 'END storage format tools' "$WORK/tools.sh" || { echo "FAIL: could not extract the storage format tools block"; exit 1; }
# The install's progress events (#616), which the blocks below append to.
sed -n '/# --- BEGIN install progress/,/# --- END install progress/p' "$GEN" >> "$WORK/tools.sh"
grep -q 'END install progress' "$WORK/tools.sh" || { echo "FAIL: could not extract the install progress block"; exit 1; }
sed -n '/# --- BEGIN install progress/,/# --- END install progress/p' "$GEN" > "$WORK/progress.sh"
sed -n '/# --- BEGIN storage layout/,/# --- END storage layout/p' "$GEN" > "$WORK/layout.sh"
grep -q 'END storage layout' "$WORK/layout.sh" || { echo "FAIL: could not extract the storage layout block"; exit 1; }
sed -n '/# --- BEGIN storage format (/,/# --- END storage format$/p' "$GEN" > "$WORK/format.sh"
grep -q 'END storage format$' "$WORK/format.sh" || { echo "FAIL: could not extract the storage format block"; exit 1; }
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

# Four drives, sda..sdd, as sysfs shows them: 4096-byte blocks, P_TYPE 0
# (what a format leaves them at; a test sets otherwise).
SYS="$WORK/sys"
mkdisk() { # name
    mkdir -p "$SYS/$1/queue" "$SYS/$1/device/scsi_disk/0:0:0:0"
    echo 19532873728 > "$SYS/$1/size"
    echo 0 > "$SYS/$1/removable"
    echo 4096 > "$SYS/$1/queue/logical_block_size"
    # PROT_EN=0: not at the asked 4096 + PI type1, so a format runs (a drive
    # already at it is not formatted again, #614; a test sets it so).
    echo 0 > "$SYS/$1/device/scsi_disk/0:0:0:0/protection_type"
}
for d in sda sdb sdc sdd; do mkdisk "$d"; done

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
    [ -e "$W/plan.args" ] && cp "$W/plan.args" "$W/plan.args1"
    for a in "$@"; do printf '%s\n' "$a"; done > "$W/plan.args"
    cat "$W/plan.stderr" >&2 2>/dev/null
    if [ -e "$W/plan.n" ] && [ -e "$W/plan.out2" ]; then cat "$W/plan.out2"; else cat "$W/plan.out" 2>/dev/null; fi
    touch "$W/plan.n"
    exit "$(cat "$W/plan.rc" 2>/dev/null || echo 0)" ;;
"slab list")
    cat "$W/slabs.${3##*/}" 2>/dev/null
    exit 0 ;;
"install-report --stage")
    shift
    for a in "$@"; do printf '%s\n' "$a"; done >> "$W/report.args"
    exit 0 ;;
"image storage-lay")
    shift 2
    for a in "$@"; do printf '%s\n' "$a"; done > "$W/lay.args"
    echo "Storage: laid" >&2
    [ -e "$W/lay.rc" ] && { echo "error: laying /dev/sdb: short write" >&2; exit 1; }
    # layfail.<name>: that drive cannot be laid (#625); the others are.
    m=""; leg=""; laid=""; bad=""; why=""
    for a in "$@"; do
        [ -n "$leg" ] && { leg=""; continue; }
        case "$a" in
        --leg|--system-bytes) leg=1 ;;
        /dev/sd*)
            if [ -e "$W/layfail.${a##*/}" ]; then
                bad="$bad $a"; why="$a: clearing the front of the drive: I/O error (its format is corrupt)"
                echo "Storage: $a: NOT LAID: clearing the front of the drive: I/O error" >&2
                continue
            fi
            laid="$laid $a"; m="$m $W/dev/${a##*/}3"; touch "$W/dev/${a##*/}3" ;;
        esac
    done
    [ -n "$laid" ] || { echo "error: no drive could be laid" >&2; exit 1; }
    echo "STORAGE_LAID='${laid# }'"
    echo "STORAGE_LAY_FAILED='${bad# }'"
    echo "STORAGE_LAY_WHY='$why'"
    echo "STORAGE_MEMBERS='${m# }'"
    echo "STORAGE_REGION=9000"; echo "STORAGE_SYSTEM_REGION=800"; echo "STORAGE_BOOT_AREA=100"
    exit 0 ;;
esac
exit 2
EOF
chmod +x "$WORK/sb"

# stormdrive: a format logs its start (with its method) and end, takes a
# second, exits rc.<name>.<prot>.<method>, else rc.<name>.<prot>; a drive
# with becomes.<name> comes back as that disk (sd made a new one); pt.<name>
# is the P_TYPE it is left at.
cat > "$WORK/stormdrive" <<EOF
#!/bin/sh
W="$WORK"
EOF
cat >> "$WORK/stormdrive" <<'EOF'
dev=""; prot=""; meth=""; bsz=""
while [ $# -gt 0 ]; do
    case "$1" in
    --device) dev=$2; shift ;;
    --block-size) bsz=$2; shift ;;
    --protection) prot=$2; shift ;;
    --method) meth=$2; shift ;;
    esac
    shift
done
n=${dev##*/}
echo "start $n $prot $meth" >> "$W/format.log"
echo "formatting $n" >&2
echo "$dev: formatting (fast) 40%, about 1 min left, 1 min so far" >&2
sleep "$(cat "$W/slow.$n" 2>/dev/null || echo 1)"
rc=$(cat "$W/rc.$n.$prot.$meth" 2>/dev/null || cat "$W/rc.$n.$prot" 2>/dev/null || echo 0)
[ "$rc" = 1 ] && echo "$dev: full format: sense 03/31/00 medium format corrupted" >&2
[ "$rc" = 3 ] && echo "$dev: refused 4096 with PI type 1; nothing was formatted: mode select: sense 5/26/00 invalid field in parameter list (byte 4)"
if [ "$rc" = 0 ] && [ -e "$W/becomes.$n" ]; then
    b=$(cat "$W/becomes.$n"); S="$W/sys/$b"
    mkdir -p "$S/queue" "$S/device/scsi_disk/1:0:0:0"
    echo 19532873728 > "$S/size"; echo 0 > "$S/removable"; echo 4096 > "$S/queue/logical_block_size"
    echo 1 > "$S/device/scsi_disk/1:0:0:0/protection_type"
    cp "$W/wwn.$n" "$W/wwn.$b"
    # Now a disk with a size: no longer one with no readable format.
    touch "$W/done.$n"
fi
# A format leaves the drive at the PI it was asked (pt.<name>: at another).
[ "$rc" = 0 ] && [ -n "$bsz" ] && [ -d "$W/sys/$n/queue" ] && echo "$bsz" > "$W/sys/$n/queue/logical_block_size"
if [ "$rc" = 0 ] && [ -d "$W/sys/$n/device/scsi_disk/0:0:0:0" ]; then
    case "$prot" in none) echo 0 ;; type1) echo 1 ;; *) echo 9 ;; esac > "$W/sys/$n/device/scsi_disk/0:0:0:0/protection_type"
fi
[ -e "$W/pt.$n" ] && cat "$W/pt.$n" > "$W/sys/$n/device/scsi_disk/0:0:0:0/protection_type"
echo "end $n $prot" >> "$W/format.log"
exit "$rc"
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
STORM_INV_SG="$WORK/sg"
STORAGE_FIND_SECS=2
STORM_STORAGE_SCSI="$WORK/scsi"
STORM_STORAGE_SD_BIND="$WORK/sd-bind"
STORM_STORAGE_SCSI_HOST="$WORK/scsi_host"
W="$WORK"
EOF
cat >> "$WORK/env.sh" <<'EOF'
mkdir -p "$STORM_RUN" "$STORAGE_DEV"
drive_ident() { echo "ID-${1##*/}"; }
sd_drives() { for d in sda sdb sdc sdd; do echo "$SURVEY_SYS/$d"; done; }
shelf_of() { echo "-|0|0"; }
drive_serial() { echo "SER-${1##*/}"; }
drive_wwn() { cat "$W/wwn.${1##*/}" 2>/dev/null || true; }
inv_unready() { cat "$W/unready" 2>/dev/null | while read -r _ud _us; do [ -e "$W/done.${_ud##*/}" ] || echo "$_ud $_us"; done; }
drive_model() { echo "X378"; }
disk_of() { echo "/dev/$(echo "${1##*/}" | sed 's/[0-9]*$//')"; }
drive_signature() { cat "$W/sig.${1##*/}" 2>/dev/null || true; }
sd_list() { echo "  (the drives)"; }
ld_skip() { echo "skip $1" >> "$W/skips"; }
ld_write() { echo "ldwrite $*" >> "$W/ldwrite"; }
rescue_shell() { echo RESCUE; exit 0; }
inv_card() { :; }
inv_card_act() { return 1; }
inv_rescan() { :; }
inv_settling() { :; }
EOF

reset() {
    rm -rf "$WORK/run" "$WORK"/plan.* "$WORK"/slabs.* "$WORK"/at.* "$WORK"/rc.* \
           "$WORK"/sig.* "$WORK/format.log" "$WORK/skips" "$WORK/ldwrite" "$WORK/out" \
           "$WORK/install-config.yaml" "$WORK/dev" "$WORK"/lay.args "$WORK"/lay.rc \
           "$WORK"/raid.* "$WORK/serve.args" "$WORK"/slow.* "$WORK/report.args" \
           "$WORK"/pt.* "$WORK"/layfail.* "$WORK"/card.* "$WORK"/becomes.* "$WORK"/wwn.* "$WORK"/done.* "$WORK/unready" "$WORK/sys/sde" "$WORK/sys/sdf" "$WORK/sg" "$WORK/scsi" "$WORK/sd-bind" \
           "${WORK:?}/plan.stderr" "${WORK:?}/sys/vda" "${WORK:?}/sys/vdb" "${WORK:?}/sys/ublkb65535" "${WORK:?}/sys/loop0"
    for d in sda sdb sdc sdd; do mkdisk "$d"; done
}

layout() { # vars... -> runs the layout block, prints its variables to out
    {
        cat "$WORK/env.sh"
        for v in "$@"; do echo "$v"; done
        cat "$WORK/tools.sh"
        cat "$WORK/layout.sh"
        cat <<'EOF'
echo "=NAMED_DISK=$NAMED_DISK"
echo "=CHOSEN_BY=$CHOSEN_BY"
echo "=STORAGE_DRIVES=$STORAGE_DRIVES"
echo "=STORAGE_FORCE=$STORAGE_FORCE"
echo "=ASSIMILATE=$ASSIMILATE"
echo "=INSTALL_OVER=$INSTALL_OVER"
echo "=STORAGE_EARLY=$STORAGE_EARLY"
echo "=STORAGE_SYSTEM=$STORAGE_SYSTEM"
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
        cat "$WORK/tools.sh"
        cat "$WORK/format.sh"
        cat <<'EOF'
echo "=FORMATTED=$STORAGE_FORMATTED"
echo "=FORCE_LOCAL=$FORCE_LOCAL"
echo "=INSTALL_KEEP=$INSTALL_KEEP"
echo "=DRIVES=$STORAGE_DRIVES"
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
        cat "$WORK/progress.sh"
        echo 'storage_fail() { echo "FATAL: $1 (#573)"; ld_write failed "${LOCAL_DISK:-}" "storage: $1"; rescue_shell; }'
        cat "$WORK/lay.sh"
        cat <<'EOF'
echo "=LOCAL_DISK=$LOCAL_DISK"
echo "=SYSTEM_DISK=$SYSTEM_DISK"
echo "=SYSTEM_DISK_ID=$SYSTEM_DISK_ID"
echo "=PAIR_DISK=$PAIR_DISK"
echo "=FORCE_LOCAL=$FORCE_LOCAL"
echo "=MEMBERS=$STORAGE_MEMBERS"
echo "=DRIVES=$STORAGE_DRIVES"
echo "=SYSTEM=$STORAGE_SYSTEM"
echo "=LAID_LEGS=$STORAGE_LAID_LEGS"
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

echo "the layout: forge's install-config on a boot that is no install lays nothing (#598)"
reset
mkdir -p "$WORK/run"
printf 'storage:\n  drives: all\n' > "$WORK/run/install-config.forge.yaml"
printf "STORAGE_DRIVES='/dev/sda'\nSTORAGE_SYSTEM='/dev/sda'\n" > "$WORK/plan.out"
echo "superblock: data slab" > "$WORK/slabs.sda"
layout "$BASE" "INSTALL_CONFIG_FORGE=$WORK/run/install-config.forge.yaml" "STORM_INSTALL_TICKET=$WORK/run/install.json" \
    'SLAB=nvme-tcp://forge:4420/nqn.x?nsid=36'
check "the engine is not asked" "no" "$([ -e "$WORK/plan.args" ] && echo yes || echo no)"
check "no drive taken" "" "$(val STORAGE_DRIVES)"
has "said" "$WORK/out" "this boot is no install: the drives are booted as laid, none is selected (#598)"
hasnt "no STOP" "$WORK/out" "STOP: storage"
hasnt "nothing refused" "$WORK/skips" "refused (#573)"

echo "the layout: the same with the appliance's install ticket plans"
reset
mkdir -p "$WORK/run"
printf 'storage:\n  drives: all\n' > "$WORK/run/install-config.forge.yaml"
printf "STORAGE_DRIVES='/dev/sda'\nSTORAGE_SYSTEM='/dev/sda'\n" > "$WORK/plan.out"
echo '{}' > "$WORK/run/install.json"
layout "$BASE" "INSTALL_CONFIG_FORGE=$WORK/run/install-config.forge.yaml" "STORM_INSTALL_TICKET=$WORK/run/install.json" \
    'SLAB=nvme-tcp://forge:4420/nqn.x?nsid=36'
check "planned" "yes" "$([ -e "$WORK/plan.args" ] && echo yes || echo no)"
check "taken" "/dev/sda" "$(val STORAGE_DRIVES)"

echo "the layout: an install over a node laid with no set: its drive is its own (#598)"
reset
mkdir -p "$WORK/run"
printf 'storage:\n  drives: all\n' > "$WORK/run/install-config.forge.yaml"
printf "STORAGE_DRIVES='/dev/sda'\nSTORAGE_SYSTEM='/dev/sda'\n" > "$WORK/plan.out"
echo '{}' > "$WORK/run/install.json"
echo "superblock: data slab" > "$WORK/slabs.sda"
echo "superblock: data slab" > "$WORK/slabs.sdb"
layout "$BASE" "INSTALL_CONFIG_FORGE=$WORK/run/install-config.forge.yaml" "STORM_INSTALL_TICKET=$WORK/run/install.json" \
    'SLAB=nvme-tcp://forge:4420/nqn.x?nsid=36' "STORAGE_PLAIN_DISKS=/dev/sda"
has "this node's drive carries nothing" "$WORK/plan.args" "dev=/dev/sda;.*carries=;"
has "another drive's slabs still said" "$WORK/plan.args" "dev=/dev/sdb;.*carries=stormblock slabs (an install);"

echo "the layout: every drive the inventory lists reaches the plan, virtio too (#597)"
reset
for d in vda vdb; do mkdisk "$d"; echo "VSER-$d" > "$SYS/$d/serial"; done
mkdisk ublkb65535
mkdir -p "$SYS/loop0"; echo 100 > "$SYS/loop0/size"
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
printf "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd /dev/vda /dev/vdb'\nSTORAGE_SYSTEM='/dev/sda'\n" > "$WORK/plan.out"
printf '\033[2m2026-11-14T03:29:54.283310Z\033[0m INFO stormblock::mgmt::config: Config file not found\n' > "$WORK/plan.stderr"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml"
has "vda told to the engine" "$WORK/plan.args" "^dev=/dev/vda;"
has "vdb too" "$WORK/plan.args" "^dev=/dev/vdb;"
has "sda still" "$WORK/plan.args" "^dev=/dev/sda;"
hasnt "never the engine's own ublk device" "$WORK/plan.args" "ublkb65535"
hasnt "never a loop device" "$WORK/plan.args" "loop0"
has "each with its serial" "$WORK/plan.args" "dev=/dev/vda;serial=SER-vda;"
check "six drives laid" "/dev/sda /dev/sdb /dev/sdc /dev/sdd /dev/vda /dev/vdb" "$(val STORAGE_DRIVES)"
hasnt "the engine's log lines are not run as shell" "$WORK/out" "not found"

echo "the layout: the drives of the set this boot serves are this node's"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
printf "STORAGE_DRIVES='/dev/sda /dev/sdb'\nSTORAGE_SYSTEM='/dev/sda /dev/sdb'\n" > "$WORK/plan.out"
for d in sda sdb sdc; do echo "superblock: data slab" > "$WORK/slabs.$d"; done
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'INSTALL_OVER=/dev/sda4' "STORAGE_MEMBER_DISKS='/dev/sda /dev/sdb'"
has "a member carries nothing" "$WORK/plan.args" "dev=/dev/sdb;.*carries=;"
has "the node's own drive still carries nothing" "$WORK/plan.args" "dev=/dev/sda;.*carries=;"
has "a drive outside the set is said" "$WORK/plan.args" "dev=/dev/sdc;.*carries=stormblock slabs (an install);"

echo "the layout: the wipe word reaches the engine"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
printf "STORAGE_DRIVES='/dev/sdb'\nSTORAGE_SYSTEM='/dev/sdb'\n" > "$WORK/plan.out"
echo "superblock: data slab" > "$WORK/slabs.sda"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'ASSIMILATE=force ASSIMILATE_ASKED=force INSTALL_OVER=/dev/sda4'
has "--force passed" "$WORK/plan.args" "^--force$"
check "STORAGE_FORCE kept for the format" "1" "$(val STORAGE_FORCE)"
check "a drive it does not take is no longer this node's" "" "$(val INSTALL_OVER)"
has "and that is said" "$WORK/out" "storage: does not take it"

echo "the layout: an install ticket's force is no wipe word (#609)"
# forge1 12.86: the appliance asked an install (ASSIMILATE=force from the
# ticket), the command line said nothing, the drive carries this node's data.
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
printf "STORAGE_DRIVES='/dev/sda'\nSTORAGE_SYSTEM='/dev/sda'\n" > "$WORK/plan.out"
echo "superblock: data slab" > "$WORK/slabs.sda"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'ASSIMILATE=force INSTALL_OVER=/dev/sda4'
hasnt "no --force" "$WORK/plan.args" "^--force$"
check "no STORAGE_FORCE" "" "$(val STORAGE_FORCE)"

echo "the layout: the install-config's wipe word is the wipe word for this install (#614)"
# forge1 after #609: the appliance's ticket (ASSIMILATE=force, nothing on the
# command line), sda carrying this node's data and its set served, two drives
# with no readable format; the operator's tool wrote storage.wipe.
reset
printf 'storage:\n  drives: all\n  wipe: {by: "testhost install --force by ops", why: "four drives"}\n' > "$WORK/install-config.yaml"
cat > "$WORK/plan.out" <<'PEOF'
STORAGE_DRIVES='/dev/sda'
STORAGE_SYSTEM='/dev/sda'
STORAGE_WIPE='the install-config (testhost install --force by ops: four drives)'
PEOF
mkdir -p "$WORK/sg"
printf '/dev/sg1 %s\n' "$WORK/sg/sg1" > "$WORK/unready"
echo "superblock: data slab" > "$WORK/slabs.sda"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'ASSIMILATE=force INSTALL_OVER=/dev/sda4' \
    "STORAGE_SERVED=/dev/ublkb65535 STORAGE_MEMBER_DISKS=/dev/sda"
check "STORAGE_FORCE from the install-config" "1" "$(val STORAGE_FORCE)"
has "where the word came from is said" "$WORK/out" "storage: wipe asked by the install-config (testhost install --force by ops: four drives): every drive storage: takes is formatted"
hasnt "the first plan had no --force" "$WORK/plan.args1" "^--force$"
has "planned again with --force" "$WORK/plan.args" "^--force$"
hasnt "the first plan left the not-ready drive out (the set is served)" "$WORK/plan.args1" "dev=/dev/sg1;"
has "the second has it" "$WORK/plan.args" "dev=/dev/sg1;.*ready=0"
check "the drive taken" "/dev/sda" "$(val STORAGE_DRIVES)"
hasnt "no STOP" "$WORK/out" "STOP: storage"
hasnt "no 'not formatted on this boot' warning" "$WORK/out" "install with the wipe word to take them"

echo "the layout: the same install without the word: no force, the not-ready drive warned"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
printf "STORAGE_DRIVES='/dev/sda'\nSTORAGE_SYSTEM='/dev/sda'\nSTORAGE_WIPE=''\n" > "$WORK/plan.out"
mkdir -p "$WORK/sg"
printf '/dev/sg1 %s\n' "$WORK/sg/sg1" > "$WORK/unready"
echo "superblock: data slab" > "$WORK/slabs.sda"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'ASSIMILATE=force INSTALL_OVER=/dev/sda4' \
    "STORAGE_SERVED=/dev/ublkb65535 STORAGE_MEMBER_DISKS=/dev/sda"
check "no STORAGE_FORCE" "" "$(val STORAGE_FORCE)"
hasnt "no wipe said" "$WORK/out" "wipe asked by"
hasnt "no --force" "$WORK/plan.args" "^--force$"
check "planned once" "no" "$([ -e "$WORK/plan.args1" ] && echo yes || echo no)"

echo "the layout: assimilate=off and recovery outweigh the install-config's wipe word"
for v in 'ASSIMILATE=off' 'OVERRIDE=recovery'; do
    reset
    printf 'storage:\n  drives: all\n  wipe: all\n' > "$WORK/install-config.yaml"
    printf "STORAGE_DRIVES='/dev/sda'\nSTORAGE_WIPE='the install-config'\n" > "$WORK/plan.out"
    layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" "$v"
    check "$v: no STORAGE_FORCE" "" "$(val STORAGE_FORCE)"
    check "$v: the engine not asked" "no" "$([ -e "$WORK/plan.args" ] && echo yes || echo no)"
done

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

echo "the layout: a drive only the card lists is formatted through the card, then planned again (#611)"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
cat > "$WORK/plan.out" <<'EOF'
STORAGE_DRIVES='/dev/sda'
STORAGE_SYSTEM='/dev/sda'
STORAGE_UNREADY=''
STORAGE_BLOCK_SIZE=4096
STORAGE_PROTECTION=type1
STORAGE_METHOD=auto
STORAGE_SAID='STORAGE: 1 drive'
EOF
cat > "$WORK/plan.out2" <<'EOF'
STORAGE_DRIVES='/dev/sda /dev/sdb'
STORAGE_SYSTEM='/dev/sda /dev/sdb'
STORAGE_UNREADY=''
STORAGE_BLOCK_SIZE=4096
STORAGE_PROTECTION=type1
STORAGE_METHOD=auto
STORAGE_SAID='STORAGE: 2 drives, planned again'
EOF
CARD='inv_card() { case "$1" in pending) [ -e "$W/card.formatted" ] || echo "0000:c1:00.0:sas-0x5000cca268077301" ;; view) echo "card: drive sas 0x5000cca268077301 slot 3, SSP target, phy 3, present: ready - 0:0:3:0 sdb sg3" ;; esac; }
inv_card_act() { echo "$*" >> "$W/card.args"; touch "$W/card.formatted"; echo "0x5000cca268077301: FORMAT UNIT to 4096 with PI type 1 started (IMMED) through the card"; }'
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" "$CARD"
has "the card-only drive is said" "$WORK/out" "storage: 0x5000cca268077301 listed by the card with no SCSI device - formatted through the card to 4096 + PI type1"
has "  formatted through the card at the plan's format" "$WORK/card.args" "^--sas 0x5000cca268077301 --action format --block-size 4096 --pi type1 --wait"
has "  the card's answer on the console" "$WORK/out" "storage: 0x5000cca268077301: FORMAT UNIT to 4096 with PI type 1 started"
has "  found again, the card's row" "$WORK/out" "card: drive sas 0x5000cca268077301 slot 3, SSP target, phy 3, present: ready - 0:0:3:0 sdb sg3"
has "  and planned again" "$WORK/out" "STORAGE: 2 drives, planned again"
hasnt "  no rescue" "$WORK/out" "RESCUE"
has "  the card's rows before the format" "$WORK/out" "^    card: drive sas 0x5000cca268077301 slot 3"

cp "$WORK/plan.out" "$WORK/cardplan.1"; cp "$WORK/plan.out2" "$WORK/cardplan.2"
cardplans() { cp "$WORK/cardplan.1" "$WORK/plan.out"; cp "$WORK/cardplan.2" "$WORK/plan.out2"; }

echo "the layout: a format through the card reports as the sg path does (#595)"
# forge1 12.87: the card path said nothing for 10 min at a time, and no beat.
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
CARDSLOW='inv_card() { case "$1" in pending) [ -e "$W/card.formatted" ] || echo "0000:c1:00.0:sas-0x5000cca268077301" ;; view) echo "card: drive sas 0x5000cca268077301 slot 3" ;; esac; }
inv_card_act() { echo "0x5000cca268077301: FORMAT UNIT to 4096 with PI type 1 started (IMMED) through the card"; echo "0x5000cca268077301: formatting (full) 1.0%, about 9h 0m left, 5m so far"; sleep 3; touch "$W/card.formatted"; echo "0x5000cca268077301: formatted (full) through the card in 10m"; }'
cardplans
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'STORAGE_BEAT_SECS=1' "$CARDSLOW"
has "  each line on the console" "$WORK/out" "^  storage: 0x5000cca268077301: formatting (full) 1.0%, about 9h 0m left, 5m so far"
has "  the minute beat over the card's drive" "$WORK/out" "FORMAT: 0 min so far - 0x5000cca268077301: formatting (full) 1.0%, about 9h 0m left"
has "  the install record in formatting" "$WORK/report.args" "^formatting$"
has "  with the card's progress" "$WORK/report.args" "0x5000cca268077301: formatting (full) 1.0%"
has "  the end said" "$WORK/out" "storage: 0x5000cca268077301: formatted (full) through the card in 10m"
has "  planned again" "$WORK/out" "STORAGE: 2 drives, planned again"
has "  the page's record: the format running through the card (#616)" "$WORK/run/install-progress.events" "	step	format	running	0x5000cca268077301 through the card$"
has "  and the stage" "$WORK/run/install-progress.events" "	stage	formatting	running	through the card: 0x5000cca268077301$"

echo "the layout: a format through the card that fails fails the install, its words quoted (#595)"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
CARDBAD='inv_card() { case "$1" in pending) echo "0000:c1:00.0:sas-0x5000cca268077301" ;; view) echo "card: drive sas 0x5000cca268077301 slot 3" ;; esac; }
inv_card_act() { echo "hba act: 0x5000cca268077301: FORMAT UNIT: CHECK CONDITION ILLEGAL REQUEST asc/ascq 0x24/0x00" >&2; return 1; }'
cardplans
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" "$CARDBAD"
has "  failed, named with the sense" "$WORK/out" "the format through the card failed on 0x5000cca268077301(.*asc/ascq 0x24/0x00"
has "  the page's record: the format failed (#616)" "$WORK/run/install-progress.events" "	step	format	failed	through the card: 0x5000cca268077301(.*0x24/0x00"

reset
printf 'storage:\n  drives: [sda]\n' > "$WORK/install-config.yaml"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" "$CARD"
has "drives not all: the card-only drive is named, not taken" "$WORK/out" "WARNING: storage: the card lists drives the kernel has no device for: 0x5000cca268077301; only drives: all takes them (#611)"
check "  not formatted" "" "$(cat "$WORK/card.args" 2>/dev/null)"

echo "the layout: forge1 - drives with no readable format are formatted, then planned again (#595)"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
mkdir -p "$WORK/sg/sg1" "$WORK/sg/sg2"
printf '/dev/sg1 %s\n/dev/sg2 %s\n' "$WORK/sg/sg1" "$WORK/sg/sg2" > "$WORK/unready"
echo naa.5000cca26801b648 > "$WORK/wwn.sg1"; echo sdf > "$WORK/becomes.sg1"
echo naa.5000cca26a819e68 > "$WORK/wwn.sg2"; echo sde > "$WORK/becomes.sg2"
# sg1's SCSI device, which sd made no disk of: bound to sd after the format
# (#611); sg2 has none, so every SCSI host is scanned.
mkdir -p "$WORK/scsi/0:0:1:0"; echo naa.5000cca26801b648 > "$WORK/scsi/0:0:1:0/wwid"
cat > "$WORK/plan.out" <<'EOF'
STORAGE_DRIVES='/dev/sda /dev/sg1 /dev/sg2'
STORAGE_SYSTEM='/dev/sda /dev/sg1 /dev/sg2'
STORAGE_UNREADY='/dev/sg1 /dev/sg2'
STORAGE_BLOCK_SIZE=4096
STORAGE_PROTECTION=type1
STORAGE_METHOD=auto
STORAGE_SAID='STORAGE: 3 drives'
EOF
cat > "$WORK/plan.out2" <<'EOF'
STORAGE_DRIVES='/dev/sda /dev/sde /dev/sdf'
STORAGE_SYSTEM='/dev/sda /dev/sde /dev/sdf'
STORAGE_UNREADY=''
STORAGE_BLOCK_SIZE=4096
STORAGE_PROTECTION=type1
STORAGE_METHOD=auto
STORAGE_SAID='STORAGE: 3 drives, planned again'
EOF
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'sd_drives() { for d in "$SURVEY_SYS"/sd*; do echo "$d"; done; }'
has "the not-ready drives told to the engine" "$WORK/plan.args1" "dev=/dev/sg1;.*wwn=naa.5000cca26801b648;.*bytes=0;.*ready=0"
has "said" "$WORK/out" "/dev/sg1 /dev/sg2 not ready (no readable format) - formatted first"
has "sg1 formatted with PI" "$WORK/format.log" "start sg1 type1 fast"
has "sg2 formatted with PI" "$WORK/format.log" "start sg2 type1 fast"
has "found again by WWID" "$WORK/out" "/dev/sg1 is /dev/sdf now (WWID naa.5000cca26801b648)"
has "the kernel asked to read it again: sd bound" "$WORK/out" "the kernel asked to read naa.5000cca26801b648 again: sd bound to 0:0:1:0 (#611)"
check "  written to sd's bind" "0:0:1:0" "$(cat "$WORK/sd-bind" 2>/dev/null)"
has "  no SCSI device: every host scanned" "$WORK/out" "the kernel asked to read naa.5000cca26a819e68 again: a scan of every SCSI host"
has "checked" "$WORK/out" "FORMATTED: /dev/sde (naa.5000cca26a819e68) reads 4096-byte blocks, PROT_EN=1 P_TYPE 1"
check "formatted this boot" "/dev/sdf /dev/sde" "$(val STORAGE_EARLY)"
has "the second plan sees them as disks" "$WORK/plan.args" "dev=/dev/sdf;"
hasnt "and none not ready" "$WORK/plan.args" "ready=0"
check "the second plan's drives" "/dev/sda /dev/sde /dev/sdf" "$(val STORAGE_DRIVES)"
has "the second plan said" "$WORK/out" "STORAGE: 3 drives, planned again"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the layout: a not-ready drive's format refused is fatal (#595)"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
mkdir -p "$WORK/sg/sg1"
printf '/dev/sg1 %s\n' "$WORK/sg/sg1" > "$WORK/unready"
echo naa.5000cca26801b648 > "$WORK/wwn.sg1"; echo 3 > "$WORK/rc.sg1.type1"
cat > "$WORK/plan.out" <<'EOF'
STORAGE_DRIVES='/dev/sda /dev/sg1'
STORAGE_SYSTEM='/dev/sda /dev/sg1'
STORAGE_UNREADY='/dev/sg1'
STORAGE_BLOCK_SIZE=4096
STORAGE_PROTECTION=type1
STORAGE_METHOD=auto
EOF
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml"
has "rescue" "$WORK/out" "RESCUE"
has "the refusal and its sense" "$WORK/out" "FATAL: the format to 4096 + PI type1 failed on /dev/sg1(refused 4096 + PI type1: .*26/00"

echo "the layout: a node with its set served leaves a not-ready drive alone"
reset
printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
mkdir -p "$WORK/sg/sg1"
printf '/dev/sg1 %s\n' "$WORK/sg/sg1" > "$WORK/unready"
printf "STORAGE_DRIVES='/dev/sda /dev/sdb'\nSTORAGE_SYSTEM='/dev/sda /dev/sdb'\n" > "$WORK/plan.out"
layout "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml" 'STORAGE_SERVED=/dev/ublkb65535'
hasnt "not told to the engine" "$WORK/plan.args" "sg1"
has "warned" "$WORK/out" "WARNING: storage: /dev/sg1 not ready (no readable format) and this node's set is served"
check "no format ran" "no" "$([ -e "$WORK/format.log" ] && echo yes || echo no)"

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

echo "the format: a beat while drives format"
reset
echo 4 > "$WORK/slow.sdb"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'STORAGE_BEAT_SECS=2'
check "both formatted" "/dev/sda /dev/sdb" "$(val FORMATTED)"
has "the beat names each drive" "$WORK/out" "FORMAT: 0 min so far - sda: ended; sdb: formatting (fast) 40%"
has "the state is formatting" "$WORK/ldwrite" "ldwrite formatting /dev/sda4 storage: formatting, 0 min so far - "
has "the appliance told" "$WORK/report.args" "^formatting$"
has "with the progress" "$WORK/report.args" "sdb: formatting (fast) 40%"
has "the page's record: the format ran and ended (#616)" "$WORK/run/install-progress.events" "	step	format	running	/dev/sda /dev/sdb to 4096 + PI type1$"
has "  done" "$WORK/run/install-progress.events" "	step	format	done	/dev/sda /dev/sdb$"
check "the beat ended with the formats" "" "$(sed -n '/=FORMATTED=/,$p' "$WORK/out" | grep 'FORMAT: ' || true)"

echo "the format: a drive already at 4096 + PI type1 is not formatted again, the rest are (#614)"
# Owner, #614: "skip the format on drives already at 4096 + PI". forge1: sda
# at 4096 + PI type 1 after its 14 h FORMAT UNIT, the others not.
reset
echo 1 > "$SYS/sda/device/scsi_disk/0:0:0:0/protection_type"
echo 512 > "$SYS/sdb/queue/logical_block_size"
format "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc'"
has "the skip said" "$WORK/out" "  sda: already 4096 + PI type1 (READ CAPACITY 16), not formatted again (#614)"
hasnt "no FORMAT UNIT for sda" "$WORK/format.log" "start sda "
has "sdb at 512 formatted" "$WORK/format.log" "start sdb type1 fast"
has "sdc without PI formatted" "$WORK/format.log" "start sdc type1 fast"
has "the FORMAT line names only those" "$WORK/out" "FORMAT: /dev/sdb /dev/sdc to 4096"
has "sda read back all the same" "$WORK/out" "FORMATTED: /dev/sda reads 4096-byte blocks, PROT_EN=1 P_TYPE 1 (READ CAPACITY 16)"
has "sdb read back at 4096" "$WORK/out" "FORMATTED: /dev/sdb reads 4096-byte blocks, PROT_EN=1 P_TYPE 1"
check "all three ready to lay" "/dev/sda /dev/sdb /dev/sdc" "$(val FORMATTED)"
check "sda's record says it was kept at its format" "already type1 none" "$(cat "$WORK/run/storage-format/sda.fmt" 2>/dev/null)"
check "the fresh drives still forced" "1" "$(val FORCE_LOCAL)"

echo "the format: every drive at the format: nothing formatted, no stormdrive needed"
reset
for d in sda sdb; do echo 1 > "$SYS/$d/device/scsi_disk/0:0:0:0/protection_type"; done
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" "STORM_STORMDRIVE=$WORK/none"
check "no format ran" "no" "$([ -e "$WORK/format.log" ] && echo yes || echo no)"
hasnt "no rescue" "$WORK/out" "RESCUE"
check "both ready to lay" "/dev/sda /dev/sdb" "$(val FORMATTED)"

echo "the format: protection none asks P_TYPE 0"
reset
format "STORAGE_DRIVES='/dev/sda'" 'STORAGE_PROTECTION=none'
has "said" "$WORK/out" "sda: already 4096 + no PI (READ CAPACITY 16), not formatted again"

echo "the format: a drive formatted (#595)"
reset
format "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc'"
check "all three formatted" "/dev/sda /dev/sdb /dev/sdc" "$(val FORMATTED)"
has "fast first" "$WORK/format.log" "start sda type1 fast"
has "each checked as the kernel reads it" "$WORK/out" "FORMATTED: /dev/sda reads 4096-byte blocks, PROT_EN=1 P_TYPE 1 (READ CAPACITY 16)"
hasnt "never --method none" "$WORK/format.log" " none$"

echo "the format: blank drives at the format on a keep-data reinstall are formatted"
reset
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_KEEP=1 INSTALL_FRESH=""'
check "both formatted" "/dev/sda /dev/sdb" "$(val FORMATTED)"
check "nothing kept now" "" "$(val INSTALL_KEEP)"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the format: the fast format refused -> the full one, same format"
reset
echo 3 > "$WORK/rc.sdb.type1.fast"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'"
check "both formatted" "/dev/sda /dev/sdb" "$(val FORMATTED)"
has "sdb full with PI" "$WORK/format.log" "start sdb type1 full"
hasnt "sda not again" "$WORK/format.log" "start sda type1 full"

echo "the format: 4096 + PI refused is fatal - never 512, never without PI (#595)"
reset
echo 3 > "$WORK/rc.sdb.type1"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'"
has "rescue" "$WORK/out" "RESCUE"
has "the sense quoted" "$WORK/out" "FATAL: the format to 4096 + PI type1 failed on /dev/sdb(refused 4096 + PI type1: .*sense 5/26/00 invalid field in parameter list"
hasnt "no PI none tried" "$WORK/format.log" "start sdb none"
has "recorded for the drive" "$WORK/run/storage-format/sdb.fmt" "^refused type1: "
has "the install result says failed" "$WORK/ldwrite" "ldwrite failed /dev/sda4 storage: the format to 4096"

echo "the format: a drive that reads no PI after its format is fatal"
reset
echo 0 > "$WORK/pt.sdc"
format "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc'"
has "rescue" "$WORK/out" "RESCUE"
has "said" "$WORK/out" "FATAL: /dev/sdc reads P_TYPE 0 (protection_type) after its format to PI type1"

echo "the format: a failure is fatal, and nothing is laid"
reset
echo 1 > "$WORK/rc.sdc.type1"
format "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc'"
has "rescue" "$WORK/out" "RESCUE"
has "names the drive and its sense" "$WORK/out" "FATAL: the format to 4096 + PI type1 failed on /dev/sdc(exit 1: .*sense 03/31/00 medium format corrupted)"
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

echo "the format: a keep-data reinstall over this node's layout keeps it"
reset
for d in sda sdb; do mkdir -p "$SYS/$d/queue"; echo 4096 > "$SYS/$d/queue/logical_block_size"; echo "superblock: data slab" > "$WORK/slabs.$d"; done
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_KEEP=1 INSTALL_FRESH=""' "STORAGE_SERVED=/dev/ublkb65535 STORAGE_MEMBER_DISKS='/dev/sda /dev/sdb'"
has "KEEP said" "$WORK/out" "KEEP: /dev/sda /dev/sdb carry this node's layout"
hasnt "no STOP" "$WORK/out" "STOP"
check "nothing formatted" "" "$(val FORMATTED)"
check "no format ran" "no" "$([ -e "$WORK/format.log" ] && echo yes || echo no)"
check "keep-data stands" "1" "$(val INSTALL_KEEP)"

echo "the format: a keep-data reinstall over another block size stops"
reset
echo 512 > "$SYS/sdb/queue/logical_block_size"
echo "superblock: data slab" > "$WORK/slabs.sda"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_KEEP=1 INSTALL_FRESH=""' "STORAGE_SERVED=/dev/ublkb65535 STORAGE_MEMBER_DISKS='/dev/sda /dev/sdb'"
has "what differs" "$WORK/out" "differs from what the drives carry: /dev/sdb(512-byte blocks)"
has "STOP" "$WORK/out" "STOP: storage:"
echo 4096 > "$SYS/sdb/queue/logical_block_size"

echo "the format: a keep-data reinstall taking a drive outside the set stops"
reset
echo "superblock: data slab" > "$WORK/slabs.sda"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_KEEP=1 INSTALL_FRESH=""' "STORAGE_SERVED=/dev/ublkb65535 STORAGE_MEMBER_DISKS='/dev/sda /dev/sdc'"
has "what differs" "$WORK/out" "/dev/sdb(not in the node's set) /dev/sdc(in the set, not taken)"
has "STOP" "$WORK/out" "STOP: storage:"

echo "the format: the wipe word relays even this node's layout"
reset
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_KEEP=1 INSTALL_FRESH="" STORAGE_FORCE=1' "STORAGE_SERVED=/dev/ublkb65535 STORAGE_MEMBER_DISKS='/dev/sda /dev/sdb'"
hasnt "no KEEP" "$WORK/out" "KEEP:"
check "both formatted" "/dev/sda /dev/sdb" "$(val FORMATTED)"

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

echo "the format: a keep-data reinstall over this node's own data stops, nothing formatted (#609)"
reset
echo "superblock: data slab" > "$WORK/slabs.sda"
format "STORAGE_DRIVES='/dev/sda'" 'INSTALL_KEEP=1 INSTALL_FRESH=1 STORAGE_FORCE=""'
has "STOP" "$WORK/out" "STOP: storage:"
has "names the drive" "$WORK/out" "/dev/sda(stormblock slabs)"
check "no format ran" "no" "$([ -e "$WORK/format.log" ] && echo yes || echo no)"
has "rescue" "$WORK/out" "RESCUE"

echo "the format: the wipe word over this node's data goes ahead"
reset
echo "superblock: data slab" > "$WORK/slabs.sda"
format "STORAGE_DRIVES='/dev/sda /dev/sdb'" 'INSTALL_KEEP=1 INSTALL_FRESH="" STORAGE_FORCE=1'
check "both formatted" "/dev/sda /dev/sdb" "$(val FORMATTED)"
check "nothing kept now" "" "$(val INSTALL_KEEP)"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the format: the install-config's wipe word over this node's own data goes ahead, naming what is lost (#614)"
# The keep-data case that STOPs above (#609), with storage.wipe.
reset
echo "superblock: data slab" > "$WORK/slabs.sda"
format "STORAGE_DRIVES='/dev/sda'" 'INSTALL_KEEP=1 INSTALL_FRESH=1 STORAGE_FORCE=1' \
    "STORAGE_WIPE='the install-config (testhost install --force by ops: four drives)'"
has "the wipe line" "$WORK/out" "storage: wipe asked by the install-config (testhost install --force by ops: four drives): /dev/sda(stormblock slabs) laid afresh at 4096 + PI type1, what it holds is lost (#614)"
hasnt "no STOP" "$WORK/out" "STOP"
check "formatted" "/dev/sda" "$(val FORMATTED)"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the format: forge1 under the wipe word: sda at 4096 + PI type1 skipped, still taken (#614)"
reset
echo "superblock: data slab" > "$WORK/slabs.sda"
echo 1 > "$SYS/sda/device/scsi_disk/0:0:0:0/protection_type"
format "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd'" 'INSTALL_KEEP=1 INSTALL_FRESH=1 STORAGE_FORCE=1' \
    "STORAGE_WIPE='the install-config'"
has "what is lost said" "$WORK/out" "wipe asked by the install-config: /dev/sda(stormblock slabs) laid afresh"
has "sda not formatted again" "$WORK/out" "sda: already 4096 + PI type1 (READ CAPACITY 16), not formatted again"
check "three formats" "3" "$(grep -c '^start' "$WORK/format.log")"
check "all four laid next" "/dev/sda /dev/sdb /dev/sdc /dev/sdd" "$(val FORMATTED)"
check "nothing kept" "" "$(val INSTALL_KEEP)"

echo "the format: the STOP names the install-config's wipe word"
reset
echo "superblock: data slab" > "$WORK/slabs.sda"
format "STORAGE_DRIVES='/dev/sda'" 'INSTALL_KEEP=1 INSTALL_FRESH=1 STORAGE_FORCE=""'
has "how to say it" "$WORK/out" "storage.wipe: all in this install's install-config"

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
has "served at id 65535" "$WORK/serve.args" "^65535$"
check "the data half on the set" "$WORK/dev/ublkb65535" "$(val LOCAL_DISK)"
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

echo "the lay: a kept layout lays nothing"
reset
echo "superblock: data slab" > "$WORK/slabs.sdc"
lay "$D4" 'INSTALL_KEEP=1 INSTALL_FRESH="" STORAGE_KEPT=1'
check "nothing laid" "no" "$([ -e "$WORK/lay.args" ] && echo yes || echo no)"
hasnt "no STOP" "$WORK/out" "STOP"
check "no new set" "no" "$([ -e "$WORK/raid.args" ] && echo yes || echo no)"

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

echo "the lay: a drive that cannot be laid stops the install by default (#625)"
reset
touch "$WORK/layfail.sdb"
lay "$D4"
has "STOP names it" "$WORK/out" "STOP: /dev/sdb could not be laid"
has "names the others laid" "$WORK/out" "/dev/sda /dev/sdc /dev/sdd laid"
has "names the way on" "$WORK/out" "storage.on_drive_failure: continue"
has "FATAL with why" "$WORK/out" "FATAL: laying /dev/sdb failed: /dev/sdb: clearing the front of the drive: I/O error (its format is corrupt)"
has "the engine's block shown" "$WORK/out" "Storage: /dev/sdb: NOT LAID"
has "rescue" "$WORK/out" "RESCUE"
has "in the record" "$WORK/run/install-progress.events" "drive	/dev/sdb	failed	not laid: /dev/sdb: clearing"
check "no set" "no" "$([ -e "$WORK/raid.args" ] && echo yes || echo no)"

echo "the lay: on_drive_failure continue lays the install without it (#625)"
reset
touch "$WORK/layfail.sdb"
lay "$D4" 'STORAGE_ON_DRIVE_FAILURE=continue'
hasnt "no rescue" "$WORK/out" "RESCUE"
has "warned" "$WORK/out" "WARNING: storage: /dev/sdb left out (storage.on_drive_failure: continue)"
has "the set said" "$WORK/out" "legs: /dev/sda /dev/sdc /dev/sdd, the set 2 data + 1 parity (#625)"
check "three members" "3" "$(grep -c "^$WORK/dev/sd.3$" "$WORK/raid.args")"
hasnt "not sdb" "$WORK/raid.args" "sdb3"
check "the drives" "/dev/sda /dev/sdc /dev/sdd" "$(val DRIVES)"
check "the legs" "/dev/sda /dev/sdc /dev/sdd" "$(val SYSTEM)"
check "the other legs" "/dev/sdc /dev/sdd" "$(val LAID_LEGS)"
check "the data half on the set" "$WORK/dev/ublkb65535" "$(val LOCAL_DISK)"
has "in the record" "$WORK/run/install-progress.events" "drive	/dev/sdb	failed"

echo "the lay: continue - a spare takes the lost drive's place"
reset
touch "$WORK/layfail.sdb"
lay "$D4" 'STORAGE_ON_DRIVE_FAILURE=continue' 'STORAGE_SPARES=1'
check "three members" "3" "$(grep -c "^$WORK/dev/sd.3$" "$WORK/raid.args")"
hasnt "no spare left" "$WORK/raid.args" "^--spare$"
has "said" "$WORK/out" "the set 2 data + 1 parity (#625)"

echo "the lay: continue - the first leg lost, the next takes the system half"
reset
touch "$WORK/layfail.sda"
lay "$D4" 'STORAGE_ON_DRIVE_FAILURE=continue'
check "system half" "/dev/sdb" "$(val SYSTEM_DISK)"
hasnt "no rescue" "$WORK/out" "RESCUE"

echo "the lay: continue - but not past the plan"
reset
touch "$WORK/layfail.sdb"
lay "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc' STORAGE_SYSTEM='/dev/sdb'" 'STORAGE_ON_DRIVE_FAILURE=continue'
has "the only leg lost" "$WORK/out" "FATAL: every system leg (/dev/sdb) failed to lay"
reset
touch "$WORK/layfail.sdb" "$WORK/layfail.sdc"
lay "$D4" 'STORAGE_ON_DRIVE_FAILURE=continue' 'STORAGE_PARITY=2'
has "the set cannot hold" "$WORK/out" "FATAL: without /dev/sdb /dev/sdc, 2 drive(s) cannot hold a parity 2 set"
check "no set" "no" "$([ -e "$WORK/raid.args" ] && echo yes || echo no)"
reset
touch "$WORK/layfail.sda" "$WORK/layfail.sdb" "$WORK/layfail.sdc" "$WORK/layfail.sdd"
lay "$D4" 'STORAGE_ON_DRIVE_FAILURE=continue'
has "none laid is fatal" "$WORK/out" "FATAL: laying the drives failed: error: no drive could be laid"

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
