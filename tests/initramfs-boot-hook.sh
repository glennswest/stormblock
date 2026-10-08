#!/bin/sh
# The initramfs boot hook: what /init does with what a hook says (#109).
#
# The hook answers three questions the local-slab probe cannot — the slab may
# be on a disk the cmdline does not name, a slab is not the same as a bootable
# disk, and nothing in a superblock says whose disk it is — so it runs first
# and /init honours it. This pins the honouring.
#
# The sharpest case here is the one that looks like paperwork: a hook's stdout
# is `KEY='value'` lines, and /init is PID 1. Hand that to `eval` and a stray
# log line becomes a command run as root before there is a system to run it
# on. So the test installs a hook that prints one and checks it did not run.
#
# Runs the real code: the block is extracted from the init script this repo
# generates, between its two marker comments, so the test cannot drift from
# what ships.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN boot hook/,/# --- END boot hook/p' "$GEN" > "$WORK/hook.sh"
[ -s "$WORK/hook.sh" ] || { echo "FAIL: could not extract the boot hook block"; exit 1; }
sed -n '/# --- BEGIN local-slab probe/,/# --- END local-slab probe/p' "$GEN" > "$WORK/probe.sh"
[ -s "$WORK/probe.sh" ] || { echo "FAIL: could not extract the local-slab probe"; exit 1; }
sed -n '/# --- BEGIN hook takeable/,/# --- END hook takeable/p' "$GEN" > "$WORK/takeable.sh"
[ -s "$WORK/takeable.sh" ] || { echo "FAIL: could not extract the takeable block"; exit 1; }
sed -n '/# --- BEGIN assimilate survey/,/# --- END assimilate survey/p' "$GEN" > "$WORK/survey.sh"
[ -s "$WORK/survey.sh" ] || { echo "FAIL: could not extract the assimilate survey"; exit 1; }
sed -n '/# --- BEGIN boothost claim/,/# --- END boothost claim/p' "$GEN" > "$WORK/claim.sh"
[ -s "$WORK/claim.sh" ] || { echo "FAIL: could not extract the boothost claim"; exit 1; }
sed -n '/# --- BEGIN boot identity/,/# --- END boot identity/p' "$GEN" > "$WORK/identity.sh"
[ -s "$WORK/identity.sh" ] || { echo "FAIL: could not extract the boot identity block"; exit 1; }
GUID=$(sed -n 's/^STORMBOOT_GUID="\(.*\)"$/\1/p' "$WORK/identity.sh")
[ -n "$GUID" ] || { echo "FAIL: no STORMBOOT_GUID in the boot identity block"; exit 1; }

# A fake SMBIOS: the MicroCloud chassis serial every X9 blade reports.
DMI="$WORK/dmi"; mkdir -p "$DMI"
echo "S11075924402016" > "$DMI/product_serial"
echo "00000000-0000-0000-0000-0cc47a000008" > "$DMI/product_uuid"
NOVARS="$WORK/efivars.none"; mkdir -p "$NOVARS"

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}

hooks_dir() { # name -> a fresh, empty hook directory
    d="$WORK/hooks.$1"
    rm -rf "$d"; mkdir -p "$d"
    echo "$d"
}

install_hook() { # dir name body...
    d=$1; n=$2; shift 2
    printf '#!/bin/sh\n%s\n' "$*" > "$d/$n"
    chmod +x "$d/$n"
}

# Sources the shipped block and reports what it decided:
#   <HOOK_DECIDED>|<SLAB>|<VOLUME>
decide() { # hooks-dir [initial SLAB] [initial VOLUME] [legacy hook path]
    (
        set +e
        STORM_BOOT_HOOK_DIR="$1"
        STORM_BOOT_HOOK_LEGACY="${4:-$WORK/no-such-legacy-hook}"
        export STORM_BOOT_HOOK_DIR STORM_BOOT_HOOK_LEGACY
        SLAB="${2:-}"
        VOLUME="${3:-}"
        . "$WORK/hook.sh" >/dev/null 2>&1
        echo "${HOOK_DECIDED:-}|$SLAB|$VOLUME"
    )
}

# A device that is really there, since a hook naming one that is not must be
# ignored rather than believed.
REAL="$WORK/real-slab.img"
: > "$REAL"

# The same, reporting the drive the hook offered to assimilate onto.
offered() { # hooks-dir -> HOOK_TAKEABLE
    (
        set +e
        STORM_BOOT_HOOK_DIR="$1"
        STORM_BOOT_HOOK_LEGACY="$WORK/no-such-legacy-hook"
        export STORM_BOOT_HOOK_DIR STORM_BOOT_HOOK_LEGACY
        SLAB=""
        VOLUME=""
        . "$WORK/hook.sh" >/dev/null 2>&1
        echo "${HOOK_TAKEABLE:-}"
    )
}

echo "initramfs boot hook:"

# Nothing installed: /init behaves exactly as it did before the hook existed.
d=$(hooks_dir none)
check "no hook installed leaves the cmdline slab alone" "|/dev/sda2|" \
    "$(decide "$d" /dev/sda2)"

# boot-local, with a slab that exists.
d=$(hooks_dir local)
install_hook "$d" 10-yes "printf \"ZB_ACTION='boot-local'\nZB_SLAB='$REAL'\nZB_DRIVE='/dev/sdb'\n\"; exit 0"
check "boot-local replaces the cmdline's guess" "local|$REAL|" \
    "$(decide "$d" /dev/sda2)"

# ask-appliance clears the slab, which is what sends /init to boot-claim.
d=$(hooks_dir ask)
install_hook "$d" 10-ask "printf \"ZB_ACTION='ask-appliance'\nZB_REASON='no loader entry'\n\"; exit 2"
check "ask-appliance clears the slab" "appliance||" "$(decide "$d" /dev/sda2)"

# An error is not a decision: the probe below still gets to decide.
d=$(hooks_dir err)
install_hook "$d" 10-err "printf \"ZB_ACTION='error'\nZB_REASON='cannot read /sys/block'\n\"; exit 1"
check "an erroring hook falls through to the probe" "|/dev/sda2|" \
    "$(decide "$d" /dev/sda2)"

# **Nothing the hook prints is executed.** A hook that logs to stdout by
# mistake is ignored, not run.
d=$(hooks_dir evil)
install_hook "$d" 10-chatty \
    "printf \"probing disks...\ntouch $WORK/EXECUTED\nZB_ACTION='boot-local'\nZB_SLAB='$REAL'\n\"; exit 0"
got=$(decide "$d" /dev/sda2)
check "a stray line on stdout is not executed by PID 1" "no" \
    "$([ -e "$WORK/EXECUTED" ] && echo yes || echo no)"
check "and the assignments around it are still read" "local|$REAL|" "$got"

# A hook that says boot-local and names nothing, or names a device that is not
# on this machine, is ignored — believing it would trade the appliance
# fallback for a drop to an initramfs shell.
d=$(hooks_dir noslab)
install_hook "$d" 10-empty "printf \"ZB_ACTION='boot-local'\n\"; exit 0"
check "boot-local with no slab is ignored" "|/dev/sda2|" "$(decide "$d" /dev/sda2)"

d=$(hooks_dir ghost)
install_hook "$d" 10-ghost "printf \"ZB_ACTION='boot-local'\nZB_SLAB='$WORK/not-here'\n\"; exit 0"
check "boot-local naming a device that is not here is ignored" "|/dev/sda2|" \
    "$(decide "$d" /dev/sda2)"

# An exit code and an action that disagree: the code is not enough on its own.
d=$(hooks_dir liar)
install_hook "$d" 10-liar "printf \"ZB_ACTION='ask-appliance'\nZB_SLAB='$REAL'\n\"; exit 0"
check "exit 0 claiming ask-appliance is ignored" "|/dev/sda2|" "$(decide "$d" /dev/sda2)"

# A remote slab is a URI, never a path, and must not be tested for existence.
d=$(hooks_dir remote)
install_hook "$d" 10-remote \
    "printf \"ZB_ACTION='boot-local'\nZB_SLAB='nvme-tcp://10.0.0.1:4420/nqn.x?nsid=1'\n\"; exit 0"
check "a fabric URI is taken as it stands" "local|nvme-tcp://10.0.0.1:4420/nqn.x?nsid=1|" \
    "$(decide "$d")"

# Order, and stopping at the first decision.
d=$(hooks_dir order)
install_hook "$d" 10-err "printf \"ZB_ACTION='error'\nZB_REASON='not my problem'\n\"; exit 1"
install_hook "$d" 20-yes "printf \"ZB_ACTION='boot-local'\nZB_SLAB='$REAL'\n\"; exit 0"
install_hook "$d" 30-ask "printf \"ZB_ACTION='ask-appliance'\n\"; exit 2"
check "runs in order, past an error, and stops at the first decision" "local|$REAL|" \
    "$(decide "$d" /dev/sda2)"

# A file that is not executable is not a hook.
d=$(hooks_dir notexec)
install_hook "$d" 10-yes "printf \"ZB_ACTION='boot-local'\nZB_SLAB='$REAL'\n\"; exit 0"
chmod -x "$d/10-yes"
check "a non-executable file in boot.d is skipped" "|/dev/sda2|" "$(decide "$d" /dev/sda2)"

# /sbin/zeroboot is tried last, so the hook that exists today works with
# nothing installed in boot.d.
d=$(hooks_dir legacyempty)
legacy="$WORK/zeroboot"
cat > "$legacy" <<LEGACY
#!/bin/sh
echo "ZB_ACTION='boot-local'"
echo "ZB_SLAB='$REAL'"
exit 0
LEGACY
chmod +x "$legacy"
check "the legacy /sbin/zeroboot path is tried" "local|$REAL|" \
    "$(decide "$d" /dev/sda2 "" "$legacy")"

# The hook says where; the cmdline says which. An operator who named a volume
# keeps it.
d=$(hooks_dir vol)
install_hook "$d" 10-vol \
    "printf \"ZB_ACTION='boot-local'\nZB_SLAB='$REAL'\nZB_VOLUME='stormpump'\n\"; exit 0"
check "the hook names the boot volume when nothing else did" "local|$REAL|stormpump" \
    "$(decide "$d")"
check "and the cmdline's volume wins when there is one" "local|$REAL|myroot" \
    "$(decide "$d" "" myroot)"

# A drive to assimilate onto travels with either decision (#109). In practice
# it comes with ask-appliance: a node with nothing of its own boots from the
# appliance and takes the blank drive on the way, which is one boot, not two.
d=$(hooks_dir offer)
install_hook "$d" 10-offer \
    "printf \"ZB_ACTION='ask-appliance'\nZB_REASON='nothing of ours here yet'\nZB_TAKEABLE='/dev/sdb'\n\"; exit 2"
check "ask-appliance can still name a drive to take" "/dev/sdb" "$(offered "$d")"

d=$(hooks_dir offer2)
install_hook "$d" 10-offer \
    "printf \"ZB_ACTION='boot-local'\nZB_SLAB='$REAL'\nZB_TAKEABLE='/dev/sdb'\n\"; exit 0"
check "and so can boot-local" "/dev/sdb" "$(offered "$d")"

# A hook that failed is not a hook that offered.
d=$(hooks_dir offer3)
install_hook "$d" 10-offer \
    "printf \"ZB_ACTION='error'\nZB_REASON='could not read /sys/block'\nZB_TAKEABLE='/dev/sdb'\n\"; exit 1"
check "an erroring hook offers nothing" "" "$(offered "$d")"

# ---------------------------------------------------------------------------
# The probe the hook runs ahead of: what /init does when no hook decided.
# ---------------------------------------------------------------------------

# A stub `stormblock`, so the probe can be driven without a slab or a kernel.
# `slab list` answers from the file's first line, `slab volumes` from the rest.
STUB="$WORK/stormblock"
cat > "$STUB" <<'STUBEOF'
#!/bin/sh
# $1 = slab, $2 = list|volumes|holds, $3 = device; or $1 = boot-claim
if [ "$1" = boot-claim ]; then
    [ -n "${STUB_CLAIM:-}" ] && echo "$STUB_CLAIM"
    exit 0
fi
answers="$STUB_ANSWERS/$(basename "$3")"
case "$2" in
list)    sed -n '1p' "$answers" 2>/dev/null ;;
volumes) sed -n '2,$p' "$answers" 2>/dev/null ;;
holds)   echo "holds? (stub)"; exit "${STUB_HOLDS:-2}" ;;
esac
exit 0
STUBEOF
chmod +x "$STUB"

answer_for() { # device-basename first-line rest...
    mkdir -p "$WORK/answers"
    f="$WORK/answers/$1"; shift
    printf '%s\n' "$@" > "$f"
}

probe() { # slab-path [VOLUME] [META] -> the SLAB the probe leaves behind
    (
        set +e
        STORM_STORMBLOCK="$STUB"
        STUB_ANSWERS="$WORK/answers"
        export STORM_STORMBLOCK STUB_ANSWERS
        HOOK_DECIDED=""
        SLAB="$1"
        VOLUME="${2:-}"
        META="${3:-}"
        BOOTHOST="http://boothost:9090"
        # GUESS=1: nothing handed a name down, so SMBIOS is read (#249).
        if [ -n "${GUESS:-}" ]; then BOOTTAG=""; HOSTNQN=""; else BOOTTAG="TESTTAG"; HOSTNQN="nqn.test"; fi
        BOOTTAG_FROM=""; TRUST_SMBIOS="${TRUST:-}"
        STORM_EFIVARS="$NOVARS"; STORM_DMI="$DMI"
        STORM_INSTALL_TICKET="${TICKET:-$WORK/no-ticket}"; export STORM_INSTALL_TICKET
        . "$WORK/identity.sh" >/dev/null 2>&1
        . "$WORK/claim.sh" >/dev/null 2>&1
        . "$WORK/probe.sh" >/dev/null 2>&1
        if [ -n "${SHOW_HANDED:-}" ]; then
            # What boot-local is handed for finishing a flow-over (#259).
            echo "${STORMBLOCK_BOOT_TAG:-}|${STORMBLOCK_RESUME_SOURCE:-}"
        else
            echo "$SLAB${INSTALL_OVER:+|$INSTALL_OVER}"
        fi
    )
}

echo "local-slab probe:"

# The partition case, which is what a loader entry names. Before #108 this
# ran `image inspect`, which wants a GPT and fails on a partition — so a node
# whose cmdline named /dev/sda2 asked the appliance every boot, however good
# its disk was.
part="$WORK/sda2"; : > "$part"
answer_for sda2 "$part: slab 88d5da3f-1111-2222-3333-444455556666" \
    "$part: volume stormpump (2.1 GB, 540 slots) 9f1c0000-0000-0000-0000-000000000001"
check "a partition holding the boot volume is kept" "$part" "$(probe "$part")"

# The volume can be named by uuid as well as by name.
check "the boot volume may be named by uuid" "$part" \
    "$(probe "$part" 9f1c0000-0000-0000-0000-000000000001)"

# A slab formatted and never filled: the failure #108 was filed for. It passes
# `slab list` and boots nothing.
empty="$WORK/sdb2"; : > "$empty"
answer_for sdb2 "$empty: slab 77770000-1111-2222-3333-444455556666" \
    "$empty: slab 77770000-1111-2222-3333-444455556666 holds no volumes"
check "a slab with no volumes sends the node to the appliance" "" "$(probe "$empty")"

# A slab holding somebody else's volumes is not this node's boot disk.
other="$WORK/sdc2"; : > "$other"
answer_for sdc2 "$other: slab 66660000-1111-2222-3333-444455556666" \
    "$other: volume something-else (8.4 GB, 2100 slots) 9f1c0000-0000-0000-0000-000000000002"
check "a slab without the named volume goes to the appliance" "" "$(probe "$other")"

# Cannot answer is not the same as answering no. A slab that keeps no metadata
# is trusted only when the cmdline says where the records are instead.
nometa="$WORK/sdd2"; : > "$nometa"
answer_for sdd2 "$nometa: slab 55550000-1111-2222-3333-444455556666" \
    "$nometa: slab 55550000-1111-2222-3333-444455556666 keeps no volume metadata"
check "a slab that cannot answer, with rd.stormblock.meta=, is kept" "$nometa" \
    "$(probe "$nometa" "" /var/lib/stormblock)"
check "and without one, the appliance decides" "" "$(probe "$nometa")"

# Not a slab at all, and not there at all.
notslab="$WORK/sde2"; : > "$notslab"
answer_for sde2 "$notslab: not a slab (bad slab magic)"
check "a device that is not a slab goes to the appliance" "" "$(probe "$notslab")"
check "a device that is not on this machine goes to the appliance" "" \
    "$(probe "$WORK/not-here")"

# A fabric URI is not probed at all: there is no local device to ask about.
check "a remote slab is left alone" "nvme-tcp://10.0.0.1:4420/nqn.x" \
    "$(probe nvme-tcp://10.0.0.1:4420/nqn.x)"

# A bootable local disk and a release (#236). Without a boot intent a netboot
# says nothing about install vs reboot; the release does.
URI="nvme-tcp://10.0.0.1:4420/nqn.x:boothost-TESTTAG?nsid=1"
check "no image from the appliance: the local disk boots" "$part" \
    "$(STUB_CLAIM="" STUB_HOLDS=1 probe "$part")"
check "the disk holds the assigned release: a reboot, the disk boots" "$part" \
    "$(STUB_CLAIM="$URI" STUB_HOLDS=0 probe "$part")"
check "a release the disk does not hold: an install, over this disk" "$URI|$part" \
    "$(STUB_CLAIM="$URI" STUB_HOLDS=1 probe "$part")"
check "the same release, its flow-over cut short: the disk boots (#258)" "$part" \
    "$(STUB_CLAIM="$URI" STUB_HOLDS=3 probe "$part")"
check "cannot tell which release: the disk boots as before" "$part" \
    "$(STUB_CLAIM="$URI" STUB_HOLDS=2 probe "$part")"
# #259: the engine finishes the flow-over as this machine, from the clone the
# probe just compared - never by a second claim under the SMBIOS serial.
check "cut short: boot-local is handed the name and the clone just claimed" "TESTTAG|$URI" \
    "$(SHOW_HANDED=1 STUB_CLAIM="$URI" STUB_HOLDS=3 probe "$part")"
check "held: the name is handed down, no clone to resume from" "TESTTAG|" \
    "$(SHOW_HANDED=1 STUB_CLAIM="$URI" STUB_HOLDS=0 probe "$part")"
TICKET="$WORK/install.json"; echo '{}' > "$TICKET"
check "an install the appliance asked for installs whatever the disk holds" "$URI|$part" \
    "$(STUB_CLAIM="$URI" STUB_HOLDS=0 probe "$part")"
TICKET=""
check "assimilate=off: the disk boots and nothing is asked" "$part" \
    "$(ASSIMILATE=off STUB_CLAIM="$URI" STUB_HOLDS=1 probe "$part")"
# #249: a name guessed from SMBIOS may be another machine's, and so may the
# release it claimed. The disk boots; nothing is installed over it.
check "a guessed name's release the disk does not hold: the disk boots" "$part" \
    "$(GUESS=1 STUB_CLAIM="$URI" STUB_HOLDS=1 probe "$part")"
TICKET="$WORK/install.json"; echo '{}' > "$TICKET"
check "a guessed name's install ticket: the disk boots" "$part" \
    "$(GUESS=1 STUB_CLAIM="$URI" STUB_HOLDS=0 probe "$part")"
TICKET=""
check "rd.stormblock.trust-smbios=1: a guess installs as before" "$URI|$part" \
    "$(GUESS=1 TRUST=1 STUB_CLAIM="$URI" STUB_HOLDS=1 probe "$part")"

# ---------------------------------------------------------------------------
# The machine's name: handed down by the firmware, or guessed (#249).
# ---------------------------------------------------------------------------

echo "boot identity:"

efivars() { # dir tag [nqn] -> a fake efivarfs with stormbootx's variables
    rm -rf "$1"; mkdir -p "$1"
    [ -n "$2" ] && printf '\006\000\000\000%s' "$2" > "$1/StormBootTag-$GUID"
    [ -n "${3:-}" ] && printf '\006\000\000\000%s' "$3" > "$1/StormBootHostNqn-$GUID"
    return 0
}

# What boothost_claim claims as: <tag>|<source>|<host NQN>|<what boot-claim was asked>
identity() { # efivars-dir [cmdline tag] [cmdline nqn]
    (
        set +e
        STORM_EFIVARS="$1"; STORM_DMI="$DMI"
        BOOTTAG="${2:-}"; HOSTNQN="${3:-}"; BOOTTAG_FROM=""; TRUST_SMBIOS=""
        BOOTHOST="http://boothost:9090"
        ASKED="$WORK/asked"; rm -f "$ASKED"
        cat > "$WORK/stormblock-claim" <<STUBEOF
#!/bin/sh
echo "\$*" > "$ASKED"
echo "nvme-tcp://10.0.0.1:4420/nqn.x?nsid=1"
STUBEOF
        chmod +x "$WORK/stormblock-claim"
        STORM_STORMBLOCK="$WORK/stormblock-claim"
        . "$WORK/identity.sh" >/dev/null 2>&1
        . "$WORK/claim.sh" >/dev/null 2>&1
        boothost_claim >/dev/null 2>&1
        [ "${STORMBLOCK_BOOT_TAG:-}" = "$BOOTTAG" ] || { echo "exported '${STORMBLOCK_BOOT_TAG:-}', not '$BOOTTAG'"; return; }
        echo "$BOOTTAG|$BOOTTAG_FROM|$HOSTNQN|$(cat "$ASKED" 2>/dev/null)"
    )
}

V="$WORK/efivars"
efivars "$V" server8 nqn.2026-09.lo.storm:host-server8
check "the firmware's name and NQN are what Linux claims and connects as" \
    "server8|firmware|nqn.2026-09.lo.storm:host-server8|boot-claim --boothost http://boothost:9090 --tag server8" \
    "$(identity "$V")"
efivars "$V" server8
check "a name without an NQN: the NQN follows the name" \
    "server8|firmware|nqn.2026-09.lo.storm:host-server8|boot-claim --boothost http://boothost:9090 --tag server8" \
    "$(identity "$V")"
check "the firmware's name wins over a different rd.stormblock.tag=" \
    "server8|firmware|nqn.2026-09.lo.storm:host-server8|boot-claim --boothost http://boothost:9090 --tag server8" \
    "$(identity "$V" server1)"
efivars "$V" server8 nqn.2026-09.lo.storm:host-server8
check "and its NQN over a different rd.stormblock.hostnqn=" \
    "server8|firmware|nqn.2026-09.lo.storm:host-server8|boot-claim --boothost http://boothost:9090 --tag server8" \
    "$(identity "$V" "" nqn.other)"
check "no variable: rd.stormblock.tag= is used, and is not a guess" \
    "flow-1|cmdline|nqn.2026-09.lo.storm:host-flow-1|boot-claim --boothost http://boothost:9090 --tag flow-1" \
    "$(identity "$NOVARS" flow-1)"
check "nothing handed down: the SMBIOS serial, marked a guess" \
    "S11075924402016|smbios|nqn.2026-09.lo.storm:host-S11075924402016|boot-claim --boothost http://boothost:9090 --tag S11075924402016" \
    "$(identity "$NOVARS")"
efivars "$V" 'server8;reboot'
check "a variable that is not a name is ignored" \
    "S11075924402016|smbios|nqn.2026-09.lo.storm:host-S11075924402016|boot-claim --boothost http://boothost:9090 --tag S11075924402016" \
    "$(identity "$V")"

# ---------------------------------------------------------------------------
# The drive a hook offers to assimilate onto (ZB_TAKEABLE).
# ---------------------------------------------------------------------------

takeable() { # HOOK_TAKEABLE LOCAL_DISK ASSIMILATE SLAB -> the LOCAL_DISK left behind
    (
        set +e
        HOOK_TAKEABLE="$1"
        LOCAL_DISK="${2:-}"
        ASSIMILATE="${3:-}"
        SLAB="${4:-}"
        . "$WORK/takeable.sh" >/dev/null 2>&1
        echo "$LOCAL_DISK"
    )
}

echo "hook takeable:"

drive="$WORK/sdb"; : > "$drive"

# The case from the issue: nothing of ours on this machine, a blank drive
# beside it, so the node boots from the appliance and assimilates on the way.
check "an offered drive becomes the flow-over target" "$drive" "$(takeable "$drive")"

# `off` is the one way an operator says no, so it has to mean no.
check "rd.stormblock.assimilate=off refuses the offer" "" "$(takeable "$drive" "" off)"

# A policy *named on the command line* is more specific than the hook: it is a
# scan the operator asked for, on this machine.
check "a drive an explicit policy chose is kept" "/dev/sdc" \
    "$(takeable "$drive" /dev/sdc any)"

# The default policy is not an instruction, and its scan can only ask whether a
# drive is one of ours. The hook read the drive, so its offer wins.
check "the hook beats the default scan's pick" "$drive" "$(takeable "$drive" /dev/sdc)"

# A policy that found nothing leaves the offer standing.
check "a policy that found nothing still takes the offer" "$drive" \
    "$(takeable "$drive" "" any)"

# Offered a drive that is not here.
check "a drive that is not on this machine is refused" "" \
    "$(takeable "$WORK/not-here" "" any)"

# Offered the drive this boot is running from — the one mistake that costs the
# node its root. `/dev/sda2` came from `/dev/sda`.
check "the boot drive is never taken as its own flow-over target" "" \
    "$(takeable /dev/sda "" any /dev/sda2)"

# No offer at all is what every boot without a hook looks like.
check "no offer changes nothing" "/dev/sdc" "$(takeable "" /dev/sdc any)"

echo "assimilate survey:"

# A fake /sys/block and a stub whose `slab list` prints a whole canned answer,
# so a drive can report both halves on two lines the way the real one does.
SVSTUB="$WORK/stormblock-survey"
cat > "$SVSTUB" <<'STUBEOF'
#!/bin/sh
# `slab holds`: the release check (#261), answered by STUB_HOLDS (2 = cannot say)
if [ "$2" = holds ]; then echo "holds? (stub)"; exit "${STUB_HOLDS:-2}"; fi
cat "$SURVEY_ANSWERS/$(basename "$3")" 2>/dev/null
exit 0
STUBEOF
chmod +x "$SVSTUB"

survey() { # policy slab-list-output... -> the LOCAL_DISK the survey leaves behind
    (
        set +e
        STORM_INSTALL_TICKET="${TICKET:-$WORK/no-ticket}"; export STORM_INSTALL_TICKET
        sys="$WORK/sys"; rm -rf "$sys"; mkdir -p "$sys/sda" "$WORK/survey"
        echo 0 > "$sys/sda/removable"; echo 3907029168 > "$sys/sda/size"
        ASSIMILATE="$1"; shift
        printf '%s\n' "$@" > "$WORK/survey/sda"
        # The drive itself, for the blank check (#273): zeros.
        mkdir -p "$WORK/dev"; dd if=/dev/zero of="$WORK/dev/sda" bs=1048576 count=2 2>/dev/null
        STORM_STORMBLOCK="$SVSTUB"; STORM_SYS_BLOCK="$sys"; SURVEY_ANSWERS="$WORK/survey"
        STORM_DEV="$WORK/dev"
        export STORM_STORMBLOCK STORM_SYS_BLOCK SURVEY_ANSWERS STORM_DEV
        STORM_NO_INTENT="${NOINTENT:-$WORK/no-marker}"; export STORM_NO_INTENT
        SLAB_NAMED="${NAMED:-}"
        SLAB="${BOOTING:-nvme-tcp://10.0.0.1:4420/nqn.x:vol-1?nsid=1}"
        CLAIMED="${CLAIMED_T:-}"
        INSTALL_OVER="${OVER:-}"
        BOOTTAG="TESTTAG"; BOOTTAG_FROM="${FROM:-firmware}"; TRUST_SMBIOS="${TRUST:-}"
        STORM_EFIVARS="$NOVARS"
        . "$WORK/identity.sh" >/dev/null 2>&1
        BOOTTAG_FROM="${FROM:-firmware}"
        . "$WORK/survey.sh" >/dev/null 2>&1
        echo "$LOCAL_DISK${FORCE_LOCAL:+ force}"
    )
}

SYS_ONLY="/dev/sda: slab 8dd28347-44b4-48e4-82d2-9624e8b5ac07 (role=system, tier=hot, 1841822 slots, 1841740 free, in stormblock)"
DATA_ONLY="/dev/sda: slab 11111111-2222-3333-4444-555555555555 (role=data, tier=hot, 8189 slots, 8000 free, in stormblock-data)"
SYS_HALF="/dev/sda: slab 66666666-7777-8888-9999-000000000000 (role=system, tier=hot, 16258 slots, 8000 free, in stormblock)"

# The drive stormblock#118 was about: an older flow-over left one system slab
# across the whole disk and the survey refused it on every boot.
check "a lone system slab carries no identity and is taken" "/dev/sda" \
    "$(survey any "$SYS_ONLY")"
check "a blank drive is taken" "/dev/sda" \
    "$(survey any "/dev/sda: not a slab (bad slab magic)")"
check "this node's own layout is taken" "/dev/sda" \
    "$(survey any "$DATA_ONLY" "$SYS_HALF")"
check "a lone data slab is an identity and is left" "" \
    "$(survey any "$DATA_ONLY")"
check "'blank' leaves a drive that carries any slab" "" \
    "$(survey blank "$SYS_ONLY")"
check "'off' takes nothing" "" "$(survey off "$SYS_ONLY")"
check "'force' over a lone data slab: taken, never forced (#311; boot-local refuses it)" "/dev/sda" "$(survey force "$DATA_ONLY")"

# An install the appliance asked for (#148) is `force` only over a drive with
# no data slab (#311), and never when this machine's cmdline says off.
TICKET="$WORK/install.json"; echo '{}' > "$TICKET"
check "an install ticket over a lone data slab: never forced (#311)" "/dev/sda" \
    "$(survey any "$DATA_ONLY")"
check "an install ticket under the default policy: never forced over data (#311)" "/dev/sda" \
    "$(survey "" "$DATA_ONLY")"
check "'off' still refuses an install" "" "$(survey off "$DATA_ONLY")"
TICKET=""
check "no ticket, no force" "/dev/sda" "$(survey any "$SYS_ONLY")"

# An install without an intent (#236): the appliance stated none, and this boot
# runs from the image it claimed. The old layout is not updated in place (its
# data half belongs to the release being replaced): it is forced, fresh.
CLAIM_URI="nvme-tcp://10.0.0.1:4420/nqn.x:vol-1?nsid=1"
NOINTENT="$WORK/no-intent"; : > "$NOINTENT"
# #311 (owner, 2026-10-06, superseding #261's install = wipe): the release on
# the drive decides. Another release is an install: the system half laid
# again, the data half kept (never forced); the same release, or the same
# release cut short (#258), is kept; when it cannot be told, the drive is left
# alone.
check "no intent, the disk holds another release: system half laid again, data kept (#311)" "/dev/sda" \
    "$(STUB_HOLDS=1 CLAIMED_T="$CLAIM_URI" survey any "$DATA_ONLY" "$SYS_HALF")"
check "no intent, another release on a lone data slab: not wiped (#311)" "/dev/sda" \
    "$(STUB_HOLDS=1 CLAIMED_T="$CLAIM_URI" survey "" "$DATA_ONLY")"
check "no intent, the same release cut short: kept, not forced (#258)" "/dev/sda" \
    "$(STUB_HOLDS=3 CLAIMED_T="$CLAIM_URI" survey any "$DATA_ONLY" "$SYS_HALF")"
check "no intent, the same release: kept, not forced (#261)" "/dev/sda" \
    "$(STUB_HOLDS=0 CLAIMED_T="$CLAIM_URI" survey any "$DATA_ONLY" "$SYS_HALF")"
check "no intent, the same release on a lone data slab: left, not forced (#258)" "" \
    "$(STUB_HOLDS=0 CLAIMED_T="$CLAIM_URI" survey "" "$DATA_ONLY")"
check "no intent, cannot tell the release: the node's layout is left alone (#261)" "" \
    "$(STUB_HOLDS=2 CLAIMED_T="$CLAIM_URI" survey any "$DATA_ONLY" "$SYS_HALF")"
check "cannot tell the release: even 'force' on the cmdline merges nothing" "" \
    "$(STUB_HOLDS=2 CLAIMED_T="$CLAIM_URI" survey force "$DATA_ONLY" "$SYS_HALF")"
check "a guessed name, another release: still left (#249)" "" \
    "$(STUB_HOLDS=1 FROM=smbios CLAIMED_T="$CLAIM_URI" survey any "$DATA_ONLY" "$SYS_HALF")"
check "no intent, the probe ruled an install over the node's layout: data kept (#311)" "/dev/sda" \
    "$(CLAIMED_T="$CLAIM_URI" OVER=/dev/sda survey any "$DATA_ONLY" "$SYS_HALF")"
check "no intent, a drive with no data slab: forced, nothing to keep" "/dev/sda force" \
    "$(CLAIMED_T="$CLAIM_URI" survey any "$SYS_ONLY")"
check "no intent, an install ticket: the node's layout taken, data kept (#311)" "/dev/sda" \
    "$(TICKET="$WORK/install.json" CLAIMED_T="$CLAIM_URI" survey any "$DATA_ONLY" "$SYS_HALF")"
check "no intent stated, but 'off': nothing is taken" "" \
    "$(CLAIMED_T="$CLAIM_URI" survey off "$DATA_ONLY" "$SYS_HALF")"
check "no intent stated, but booting the local disk: no force" "" \
    "$(CLAIMED_T="$CLAIM_URI" BOOTING=/dev/sda survey any "$DATA_ONLY" "$SYS_HALF")"
over="$WORK/sdz"; : > "$over"
check "the disk the probe ruled an install over is the one taken" "$over force" \
    "$(CLAIMED_T="$CLAIM_URI" OVER="$over" survey any "$SYS_ONLY")"
# #249: under a guessed name the claim, and the appliance's silence about an
# intent, may be another machine's. Only a blank drive is taken.
check "a guessed name with no intent stated: the node's layout is left" "" \
    "$(FROM=smbios CLAIMED_T="$CLAIM_URI" survey any "$DATA_ONLY" "$SYS_HALF")"
check "a guessed name: a lone system slab is left too" "" \
    "$(FROM=smbios CLAIMED_T="$CLAIM_URI" survey any "$SYS_ONLY")"
check "a guessed name: a blank drive is still taken" "/dev/sda" \
    "$(FROM=smbios CLAIMED_T="$CLAIM_URI" survey any "/dev/sda: not a slab (bad slab magic)")"
check "a guessed name: 'force' on the cmdline is not a licence" "" \
    "$(FROM=smbios CLAIMED_T="$CLAIM_URI" survey force "$DATA_ONLY" "$SYS_HALF")"
check "a guessed name, rd.stormblock.trust-smbios=1: installed, data kept (#311)" "/dev/sda" \
    "$(FROM=smbios TRUST=1 CLAIMED_T="$CLAIM_URI" OVER=/dev/sda survey any "$DATA_ONLY" "$SYS_HALF")"
check "a name given on the cmdline is not a guess: installed, data kept (#311)" "/dev/sda" \
    "$(FROM=cmdline CLAIMED_T="$CLAIM_URI" OVER=/dev/sda survey any "$DATA_ONLY" "$SYS_HALF")"
TICKET="$WORK/install.json"; echo '{}' > "$TICKET"
check "a guessed name's install ticket forces nothing" "" \
    "$(FROM=smbios survey any "$DATA_ONLY")"
TICKET=""
NOINTENT=""
# An intent other than `install` changes nothing: another release is still an
# install, and an install keeps the data half (#311).
check "an intent was stated, another release on the disk: data kept (#311)" "/dev/sda" \
    "$(STUB_HOLDS=1 CLAIMED_T="$CLAIM_URI" survey any "$DATA_ONLY" "$SYS_HALF")"
check "an intent was stated, the same release: kept, not forced" "/dev/sda" \
    "$(STUB_HOLDS=0 CLAIMED_T="$CLAIM_URI" survey any "$DATA_ONLY" "$SYS_HALF")"
check "an intent was stated, cannot tell: left alone" "" \
    "$(STUB_HOLDS=2 CLAIMED_T="$CLAIM_URI" survey any "$DATA_ONLY" "$SYS_HALF")"
check "an intent was stated, the probe ruled an install: wiped (#261)" "$over force" \
    "$(CLAIMED_T="$CLAIM_URI" OVER="$over" survey any "$SYS_ONLY")"
check "an intent was stated, no data slab anywhere: taken, not forced" "/dev/sda" \
    "$(CLAIMED_T="$CLAIM_URI" survey any "$SYS_ONLY")"

echo "drives this install may take (#273):"

# Several drives: name|kind, kind one of
#   blank       an internal drive, all zeros, no slab
#   shelf       a blank drive in a NetApp shelf: an SES enclosure behind a SAS
#               expander (a shelf's IOMs are expanders)
#   expander    a blank drive behind a SAS expander
#   bay         a blank drive in the server's own SES backplane, no expander
#               (the Dell R230's bays on its mpt3sas HBA, #344)
#   baylayout   this node's own layout, in such a bay
#   stormraid   an internal drive with a stormraid superblock
#   foreign     an internal drive with a partition table (bytes in its first MiB)
#   tail        an internal drive with data in its last MiB only (md 1.0, ZFS)
#   layout      this node's own layout (data and system slab)
#   datashelf   a stormblock data slab, in a shelf (behind an expander)
survey_m() { # policy name|kind... -> the LOCAL_DISK the survey leaves behind
    (
        set +e
        STORM_INSTALL_TICKET="${TICKET:-$WORK/no-ticket}"; export STORM_INSTALL_TICKET
        sys="$WORK/msys"; dev="$WORK/mdev"; ans="$WORK/msurvey"; real="$WORK/mreal"
        rm -rf "$sys" "$dev" "$ans" "$real"; mkdir -p "$sys" "$dev" "$ans" "$real"
        ASSIMILATE="$1"; shift
        for spec in "$@"; do
            n="${spec%%|*}"; kind="${spec#*|}"
            case "$kind" in
            expander|shelf|datashelf) mkdir -p "$real/port-0:0/expander-0:0/$n"; ln -s "$real/port-0:0/expander-0:0/$n" "$sys/$n" ;;
            *) mkdir -p "$sys/$n" ;;
            esac
            echo 0 > "$sys/$n/removable"; echo 8192 > "$sys/$n/size"
            dd if=/dev/zero of="$dev/$n" bs=1048576 count=4 2>/dev/null
            echo "/dev/$n: not a slab (bad slab magic)" > "$ans/$n"
            case "$kind" in
            shelf|datashelf|bay|baylayout) mkdir -p "$sys/$n/device/enclosure_device:Slot 03" ;;
            esac
            case "$kind" in
            stormraid) printf 'STORMRD1' | dd of="$dev/$n" conv=notrunc 2>/dev/null ;;
            foreign) printf '\125\252' | dd of="$dev/$n" bs=1 seek=510 conv=notrunc 2>/dev/null ;;
            tail) printf 'a92b4efc' | dd of="$dev/$n" bs=1 seek=$((4 * 1048576 - 4096)) conv=notrunc 2>/dev/null ;;
            layout|baylayout) printf '%s\n' "$DATA_ONLY" "$SYS_HALF" > "$ans/$n" ;;
            datashelf) printf '%s\n' "$DATA_ONLY" > "$ans/$n" ;;
            esac
        done
        STORM_STORMBLOCK="$SVSTUB"; STORM_SYS_BLOCK="$sys"; SURVEY_ANSWERS="$ans"; STORM_DEV="$dev"
        export STORM_STORMBLOCK STORM_SYS_BLOCK SURVEY_ANSWERS STORM_DEV
        STORM_NO_INTENT="${NOINTENT:-$WORK/no-marker}"; export STORM_NO_INTENT
        STORM_LOCAL_DISK_REPORT="$WORK/local-disk.json"; export STORM_LOCAL_DISK_REPORT
        rm -f "$STORM_LOCAL_DISK_REPORT"
        SLAB_NAMED="${NAMED:-}"; ALLOW_EXTERNAL="${EXTERNAL:-}"
        SLAB="${BOOTING:-nvme-tcp://10.0.0.1:4420/nqn.x:vol-1?nsid=1}"
        CLAIMED="${CLAIMED_T:-}"
        INSTALL_OVER="${OVER:-}"
        BOOTTAG="TESTTAG"; BOOTTAG_FROM=firmware; TRUST_SMBIOS=""
        STORM_EFIVARS="$NOVARS"
        . "$WORK/identity.sh" >/dev/null 2>&1
        BOOTTAG_FROM=firmware
        . "$WORK/survey.sh" > "$WORK/msurvey.log" 2>&1
        echo "$LOCAL_DISK${FORCE_LOCAL:+ force}"
    )
}

# The Dell with the NetApp shelf: rd.stormblock.slab=/dev/sda, a blank shelf
# beside it, a stormraid set on part of it.
check "the named drive is taken; the shelf is not" "/dev/sda" \
    "$(NAMED=/dev/sda survey_m any sda\|blank sdb\|shelf sdc\|stormraid)"
check "the named drive is spent: no other drive is taken instead" "" \
    "$(NAMED=/dev/sda survey_m any sda\|foreign sdb\|blank sdc\|shelf)"
grep -q "not the drive rd.stormblock.slab= names" "$WORK/msurvey.log" \
    && check "and the console says why" yes yes || check "and the console says why" yes no
check "an install over the node's layout lands on the named drive, data kept (#311)" "/dev/sda" \
    "$(NAMED=/dev/sda STUB_HOLDS=1 CLAIMED_T="$CLAIM_URI" survey_m any sda\|layout sdb\|datashelf)"
check "'force' clears the named drive whatever it carries (#236)" "/dev/sda force" \
    "$(NAMED=/dev/sda survey_m force sda\|foreign sdb\|shelf)"
NOINTENT="$WORK/no-intent"
check "no intent, a data slab only in the shelf: the shelf is not wiped" "" \
    "$(CLAIMED_T="$CLAIM_URI" survey_m any sdb\|datashelf sdc\|shelf)"
NOINTENT=""
# No drive named here (an NVMe-only machine whose line names /dev/sda).
check "none named: the internal blank drive, past the shelf and stormraid" "/dev/sdd" \
    "$(NAMED=/dev/sda survey_m any sdb\|shelf sdc\|stormraid sdd\|blank)"
check "none named: a drive behind a SAS expander is a shelf too" "" \
    "$(survey_m any sdb\|expander)"
check "none named: a stormraid member is not blank" "" "$(survey_m any sdc\|stormraid)"
grep -q "a stormraid superblock" "$WORK/msurvey.log" \
    && check "and the console names it" yes yes || check "and the console names it" yes no
check "none named: a partition table is not blank" "" "$(survey_m any sdc\|foreign)"
check "none named: data in the last MiB is not blank" "" "$(survey_m any sdc\|tail)"
check "none named, 'force': a stormraid member is still left" "" "$(survey_m force sdc\|stormraid)"
check "none named, 'force': a shelf drive with a data slab is still left" "" \
    "$(survey_m force sdb\|datashelf)"
check "rd.stormblock.allow-external=1: a blank drive in an enclosure is taken" "/dev/sdb" \
    "$(EXTERNAL=1 survey_m any sdb\|shelf)"

echo "the machine's own disk, and what the boot says about it (#344):"
ld() { tr -d '\n' < "$WORK/local-disk.json" 2>/dev/null; }
# The Dell on 11.95: no drive named, no boot intent, its own layout in a bay
# of its own SES backplane. It was taken for a shelf and the node ran from
# forge with its slabs on sda.
NOINTENT="$WORK/no-intent"; : > "$NOINTENT"
check "the Dell: its own layout in its own SES bay is installed over, data kept" "/dev/sda" \
    "$(STUB_HOLDS=1 CLAIMED_T="$CLAIM_URI" survey_m any sda\|baylayout)"
case "$(ld)" in *'"state": "taken"'*'"drive": "/dev/sda"'*) check "  and the verdict is taken, sda" yes yes ;;
*) check "  and the verdict is taken, sda: $(ld)" yes no ;; esac
check "the same release in its own bay: kept (recovery)" "/dev/sda" \
    "$(STUB_HOLDS=0 CLAIMED_T="$CLAIM_URI" survey_m any sda\|baylayout)"
NOINTENT=""
check "a blank drive in the server's own SES bay is taken" "/dev/sdb" "$(survey_m any sdb\|bay)"
# A drive with slabs left alone says which and why, and the console says it.
check "a data slab in a shelf is left" "" "$(survey_m force sdb\|datashelf)"
case "$(ld)" in *'"state": "refused"'*'"drive": "/dev/sdb"'*'SAS expander'*) check "  the verdict names the drive and the shelf" yes yes ;;
*) check "  the verdict names the drive and the shelf: $(ld)" yes no ;; esac
grep -q "WARNING: this node runs from the appliance although /dev/sdb" "$WORK/msurvey.log" \
    && check "  and the console says it" yes yes || check "  and the console says it" yes no
check "cannot tell which release: left alone" "" \
    "$(STUB_HOLDS=2 CLAIMED_T="$CLAIM_URI" survey_m any sda\|layout)"
case "$(ld)" in *'"state": "refused"'*'cannot tell which release'*) check "  the verdict says why" yes yes ;;
*) check "  the verdict says why: $(ld)" yes no ;; esac
check "no drive with slabs: the verdict is none" "" "$(survey_m any sdc\|foreign)"
case "$(ld)" in *'"state": "none"'*) check "  none" yes yes ;; *) check "  none: $(ld)" yes no ;; esac

[ "$fail" -eq 0 ] && echo "all boot hook, probe, identity, takeable and survey checks passed"
exit "$fail"
