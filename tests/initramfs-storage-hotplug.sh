#!/bin/sh
# Drives that arrive after the storage inventory (#593): the layout is planned
# again over them just before anything is written, a drive the new plan takes
# is formatted like the rest and laid with them, and one that cannot be taken
# is said with why and left to the running node. Nothing is laid differently
# from what was said.
#
# Runs the real blocks (the format tools, the layout, the arrivals, the
# watch), extracted between their markers from the init script this repo
# generates. The engine's plan and stormdrive are stand-ins that record what
# they were asked; the watch's log is written by the test, as `stormblock
# hotplug watch` would.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "${WORK:?}"' EXIT

ex() { # marker file
    sed -n "/# --- BEGIN $1 (/,/# --- END $1\$/p" "$GEN" > "$WORK/$2"
    grep -q "END $1\$" "$WORK/$2" || { echo "FAIL: could not extract the $1 block"; exit 1; }
}
ex "storage format tools" tools.sh
ex "storage layout" layout.sh
ex "storage arrivals" arrivals.sh
ex "storage watch" watch.sh

fail=0
check() { if [ "$2" = "$3" ]; then echo "  ok    $1"; else echo "  FAIL  $1: expected '$2', got '$3'"; fail=1; fi; }
has() {
    if grep -q -- "$3" "$2" 2>/dev/null; then echo "  ok    $1"; else
        echo "  FAIL  $1: no '$3' in $(basename "$2")"; sed 's/^/  | /' "$2" 2>/dev/null | tail -25; fail=1; fi
}
hasnt() {
    if grep -q -- "$3" "$2" 2>/dev/null; then echo "  FAIL  $1: '$3' in $(basename "$2")"; sed 's/^/  | /' "$2" | tail -25; fail=1
    else echo "  ok    $1"; fi
}

SYS="$WORK/sys"
mkdisk() { # name
    mkdir -p "$SYS/$1/queue" "$SYS/$1/device/scsi_disk/0:0:0:0"
    echo 19532873728 > "$SYS/$1/size"; echo 0 > "$SYS/$1/removable"
    echo 4096 > "$SYS/$1/queue/logical_block_size"
    echo 1 > "$SYS/$1/device/scsi_disk/0:0:0:0/protection_type"
}

# The engine: the Nth `image storage-plan` answers plan.N (the last one there
# after that), and is counted in plan.calls.
cat > "$WORK/sb" <<EOF
#!/bin/sh
W="$WORK"
EOF
cat >> "$WORK/sb" <<'EOF'
case "$1 $2" in
"image storage-plan")
    n=$(( $(cat "$W/plan.calls" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$W/plan.calls"
    shift 2; for a in "$@"; do printf '%s\n' "$a"; done > "$W/plan.args.$n"
    i=$n; while [ "$i" -gt 1 ] && [ ! -e "$W/plan.$i" ]; do i=$((i - 1)); done
    cat "$W/plan.$i"; exit 0 ;;
"slab list") cat "$W/slabs.${3##*/}" 2>/dev/null; exit 0 ;;
"install-report --stage") exit 0 ;;
esac
exit 2
EOF
chmod +x "$WORK/sb"

# stormdrive: logs each format; a drive with becomes.<name> comes back as
# that disk; more.<name> is a drive that arrives while it formats.
cat > "$WORK/stormdrive" <<EOF
#!/bin/sh
W="$WORK"
EOF
cat >> "$WORK/stormdrive" <<'EOF'
dev=""
while [ $# -gt 0 ]; do case "$1" in --device) dev=$2; shift ;; esac; shift; done
n=${dev##*/}
echo "start $n" >> "$W/format.log"
if [ -e "$W/becomes.$n" ]; then
    b=$(cat "$W/becomes.$n"); S="$W/sys/$b"
    mkdir -p "$S/queue" "$S/device/scsi_disk/1:0:0:0"
    echo 19532873728 > "$S/size"; echo 0 > "$S/removable"; echo 4096 > "$S/queue/logical_block_size"
    echo 1 > "$S/device/scsi_disk/1:0:0:0/protection_type"
    cp "$W/wwn.$n" "$W/wwn.$b"; touch "$W/done.$n"
fi
if [ -e "$W/more.$n" ]; then
    a=$(cat "$W/more.$n")
    echo "add /dev/$a serial=S wwid=- controller=- bay=- bytes=1 ready=1 at=1" >> "$W/run/hotplug.log"
fi
exit 0
EOF
chmod +x "$WORK/stormdrive"

cat > "$WORK/env.sh" <<EOF
SURVEY_SB="$WORK/sb"
SURVEY_SYS="$SYS"
STORM_RUN="$WORK/run"
STORM_STORMDRIVE="$WORK/stormdrive"
STORM_INV_SG="$WORK/sg"
STORAGE_FIND_SECS=2
HOTPLUG_LOG="$WORK/run/hotplug.log"
W="$WORK"
EOF
cat >> "$WORK/env.sh" <<'EOF'
mkdir -p "$STORM_RUN"
drive_ident() { echo "ID-${1##*/}"; }
sd_drives() { for d in "$SURVEY_SYS"/*; do [ -e "$d/device" ] && echo "$d"; done; }
shelf_of() { echo "-|0|0"; }
drive_serial() { echo "SER-${1##*/}"; }
drive_wwn() { cat "$W/wwn.${1##*/}" 2>/dev/null || true; }
inv_unready() { cat "$W/unready" 2>/dev/null | while read -r _ud _us; do [ -e "$W/done.${_ud##*/}" ] || echo "$_ud $_us"; done; }
drive_model() { echo "X378"; }
disk_of() { echo "/dev/$(echo "${1##*/}" | sed 's/[0-9]*$//')"; }
drive_signature() { cat "$W/sig.${1##*/}" 2>/dev/null || true; }
sd_list() { echo "  (the drives)"; }
ld_skip() { :; }
ld_write() { echo "ldwrite $*" >> "$W/ldwrite"; }
rescue_shell() { echo RESCUE; exit 0; }
EOF

reset() {
    rm -rf "${WORK:?}/run" "${WORK:?}"/plan.* "${WORK:?}"/slabs.* "${WORK:?}"/sig.* "${WORK:?}/format.log" \
           "${WORK:?}/out" "${WORK:?}"/becomes.* "${WORK:?}"/wwn.* "${WORK:?}"/done.* "${WORK:?}"/more.* \
           "${WORK:?}/unready" "${WORK:?}/sys" "${WORK:?}/sg" "${WORK:?}/ldwrite"
    for d in sda sdb sdc; do mkdisk "$d"; done
    mkdir -p "$WORK/run"
    : > "$WORK/run/hotplug.log"
    printf 'storage:\n  drives: all\n' > "$WORK/install-config.yaml"
    printf "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc'\nSTORAGE_SYSTEM='/dev/sda /dev/sdb /dev/sdc'\nSTORAGE_SAID='STORAGE: 3 drives'\n" > "$WORK/plan.1"
}
arrive() { # dev [ready]
    echo "add $1 serial=SER-${1##*/} wwid=- controller=0000:03:00.0 bay=4 bytes=10000831348736 ready=${2:-1} at=1700000000" >> "$WORK/run/hotplug.log"
}

BASE='STORAGE_BLOCK_SIZE=4096 STORAGE_PROTECTION=type1 STORAGE_METHOD=auto OVERRIDE="" ASSIMILATE="" SLAB="" NAMED_DISK="" CHOSEN_BY="" INSTALL_OVER="" LOCAL_DISK=/dev/sda4 INSTALL_FRESH=1 INSTALL_KEEP=""'
run() { # vars...
    {
        cat "$WORK/env.sh"
        echo "$BASE" "INSTALL_CONFIG_STAGED=$WORK/install-config.yaml"
        for v in "$@"; do echo "$v"; done
        cat "$WORK/tools.sh" "$WORK/layout.sh" "$WORK/arrivals.sh"
        cat <<'EOF'
echo "=DRIVES=$STORAGE_DRIVES"
echo "=SYSTEM=$STORAGE_SYSTEM"
echo "=ARRIVED=$STORAGE_ARRIVED"
echo "=LEFT=$STORAGE_LEFT"
echo "=FORMATTED=$STORAGE_FORMATTED"
EOF
    } > "$WORK/run.sh"
    sh "$WORK/run.sh" > "$WORK/out" 2>&1 || echo "EXIT $?" >> "$WORK/out"
}
val() { sed -n "s/^=$1=//p" "$WORK/out"; }
calls() { cat "$WORK/plan.calls" 2>/dev/null || echo 0; }

echo "no drive arrives: planned once, laid as planned"
reset
run
check "one plan" "1" "$(calls)"
check "the three drives" "/dev/sda /dev/sdb /dev/sdc" "$(val DRIVES)"
check "nothing arrived" "" "$(val ARRIVED)"
hasnt "nothing said" "$WORK/out" "#593"

echo "a drive arrives before the layout is written: planned again, formatted, laid with the rest"
reset
mkdisk sdd; arrive /dev/sdd
printf "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd'\nSTORAGE_SYSTEM='/dev/sda /dev/sdb /dev/sdc /dev/sdd'\nSTORAGE_SAID='STORAGE: 4 drives'\n" > "$WORK/plan.2"
run
check "planned twice" "2" "$(calls)"
has "the new drive told to the engine" "$WORK/plan.args.2" "dev=/dev/sdd;"
check "four drives" "/dev/sda /dev/sdb /dev/sdc /dev/sdd" "$(val DRIVES)"
check "four legs" "/dev/sda /dev/sdb /dev/sdc /dev/sdd" "$(val SYSTEM)"
check "arrived" "/dev/sdd" "$(val ARRIVED)"
check "it alone formatted here" "start sdd" "$(cat "$WORK/format.log" 2>/dev/null)"
check "formatted" "/dev/sdd" "$(val FORMATTED)"
has "said" "$WORK/out" "storage: /dev/sdd arrived after the inventory - planned again before anything is laid (#593)"
has "taken, said" "$WORK/out" "storage: /dev/sdd taken into the layout"
check "nothing left" "" "$(val LEFT)"

echo "a new plan that stops is not used: laid as before, the arrival said"
reset
mkdisk sdd; arrive /dev/sdd
echo "STORAGE_STOP='/dev/sdd holds another node (n2)'" > "$WORK/plan.2"
run
check "three drives" "/dev/sda /dev/sdb /dev/sdc" "$(val DRIVES)"
check "three legs" "/dev/sda /dev/sdb /dev/sdc" "$(val SYSTEM)"
check "left" "/dev/sdd" "$(val LEFT)"
check "nothing formatted" "" "$(cat "$WORK/format.log" 2>/dev/null)"
has "said with why" "$WORK/out" "stops (/dev/sdd holds another node (n2)): laid as planned before, without /dev/sdd"
hasnt "no STOP of the install" "$WORK/out" "STOP: storage"

echo "an arrival that holds slabs, on a keep-data boot: not formatted, not taken"
reset
mkdisk sdd; arrive /dev/sdd
echo "superblock: data slab" > "$WORK/slabs.sdd"
printf "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd'\nSTORAGE_SYSTEM='/dev/sda /dev/sdb'\n" > "$WORK/plan.2"
run 'INSTALL_FRESH=""'
check "three drives" "/dev/sda /dev/sdb /dev/sdc" "$(val DRIVES)"
check "nothing formatted" "" "$(cat "$WORK/format.log" 2>/dev/null)"
has "said" "$WORK/out" "the new plan takes /dev/sdd(stormblock slabs), which hold what is not to be lost"
check "left" "/dev/sdd" "$(val LEFT)"

echo "the same with the wipe word: taken"
reset
mkdisk sdd; arrive /dev/sdd
echo "superblock: data slab" > "$WORK/slabs.sdd"
printf "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd'\nSTORAGE_SYSTEM='/dev/sda /dev/sdb'\n" > "$WORK/plan.2"
run 'INSTALL_FRESH=""' 'ASSIMILATE=force'
check "four drives" "/dev/sda /dev/sdb /dev/sdc /dev/sdd" "$(val DRIVES)"
check "formatted" "start sdd" "$(cat "$WORK/format.log" 2>/dev/null)"

echo "an arrival the plan does not take: said, left"
reset
mkdisk sdd; arrive /dev/sdd
run
check "planned again" "2" "$(calls)"
check "three drives" "/dev/sda /dev/sdb /dev/sdc" "$(val DRIVES)"
has "said" "$WORK/out" "the plan does not take /dev/sdd"
check "left" "/dev/sdd" "$(val LEFT)"

echo "an arrival with no readable format: formatted first, found again, then laid"
reset
mkdir -p "$WORK/sg/sg4"
echo "/dev/sg4 $WORK/sg/sg4" > "$WORK/unready"
echo "naa.5000c500cafe" > "$WORK/wwn.sg4"
echo sdd > "$WORK/becomes.sg4"
arrive /dev/sg4 0
printf "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc'\nSTORAGE_SYSTEM='/dev/sda /dev/sdb /dev/sdc'\nSTORAGE_UNREADY='/dev/sg4'\n" > "$WORK/plan.2"
printf "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd'\nSTORAGE_SYSTEM='/dev/sda /dev/sdb /dev/sdc /dev/sdd'\n" > "$WORK/plan.3"
run 'STORAGE_SERVED=""'
check "planned three times" "3" "$(calls)"
check "formatted once, as sg4" "start sg4" "$(cat "$WORK/format.log" 2>/dev/null)"
check "four drives" "/dev/sda /dev/sdb /dev/sdc /dev/sdd" "$(val DRIVES)"
check "arrived" "/dev/sdd" "$(val ARRIVED)"
has "said" "$WORK/out" "/dev/sg4 not ready (no readable format) - formatted first, then planned again"

echo "drives that keep arriving while each one formats: three plans, then the running node's"
reset
for d in sdd sde sdf sdg; do mkdisk "$d"; done
arrive /dev/sdd
echo sde > "$WORK/more.sdd"; echo sdf > "$WORK/more.sde"; echo sdg > "$WORK/more.sdf"
printf "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd'\nSTORAGE_SYSTEM='/dev/sda'\n" > "$WORK/plan.2"
printf "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd /dev/sde'\nSTORAGE_SYSTEM='/dev/sda'\n" > "$WORK/plan.3"
printf "STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd /dev/sde /dev/sdf'\nSTORAGE_SYSTEM='/dev/sda'\n" > "$WORK/plan.4"
run
check "four plans" "4" "$(calls)"
check "three taken" "/dev/sdd /dev/sde /dev/sdf" "$(val ARRIVED)"
check "the last left" "/dev/sdg" "$(val LEFT)"
has "said" "$WORK/out" "/dev/sdg still arriving after 3 plans: left to the running node"

echo "a kept reinstall lays nothing: arrivals are not planned here"
reset
mkdisk sdd; arrive /dev/sdd
run 'STORAGE_KEPT=1'
check "one plan" "1" "$(calls)"
check "nothing arrived into it" "" "$(val ARRIVED)"

echo "the watch: started with the binary there, stopped by hotplug_stop"
cat > "$WORK/fake-sb" <<EOF
#!/bin/sh
printf '%s\n' "\$@" > "$WORK/watch.args"
exec sleep 30
EOF
chmod +x "$WORK/fake-sb"
{
    echo "STORM_STORMBLOCK=$WORK/fake-sb STORM_HOTPLUG_LOG=$WORK/run/w/hotplug.log"
    cat "$WORK/watch.sh"
    echo 'sleep 1; echo "=PID=$HOTPLUG_PID"; hotplug_stop; echo "=AFTER=$HOTPLUG_PID"'
} > "$WORK/w.sh"
sh "$WORK/w.sh" > "$WORK/out" 2>&1
has "watch asked" "$WORK/watch.args" "^watch$"
has "its log" "$WORK/watch.args" "^$WORK/run/w/hotplug.log$"
has "said on the console" "$WORK/watch.args" "^--say$"
check "the log made" "yes" "$([ -f "$WORK/run/w/hotplug.log" ] && echo yes || echo no)"
check "stopped" "" "$(val AFTER)"
{
    echo "STORM_STORMBLOCK=$WORK/none"
    cat "$WORK/watch.sh"
    echo 'echo "=PID=$HOTPLUG_PID"; hotplug_stop'
} > "$WORK/w.sh"
sh "$WORK/w.sh" > "$WORK/out" 2>&1
check "no binary, no watch" "" "$(val PID)"

if [ "$fail" = 0 ]; then
    echo "PASS: initramfs storage hotplug"
else
    echo "FAIL: initramfs storage hotplug"
    exit 1
fi
