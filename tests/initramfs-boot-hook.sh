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
# $1 = slab, $2 = list|volumes, $3 = device
answers="$STUB_ANSWERS/$(basename "$3")"
case "$2" in
list)    sed -n '1p' "$answers" 2>/dev/null ;;
volumes) sed -n '2,$p' "$answers" 2>/dev/null ;;
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
        . "$WORK/probe.sh" >/dev/null 2>&1
        echo "$SLAB"
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

# A policy that already chose is more specific than the hook: it is a scan the
# operator asked for, on this machine.
check "a drive the policy chose is kept" "/dev/sdc" "$(takeable "$drive" /dev/sdc any)"

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

[ "$fail" -eq 0 ] && echo "all boot hook, probe and takeable checks passed"
exit "$fail"
