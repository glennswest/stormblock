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

[ "$fail" -eq 0 ] && echo "all boot hook checks passed"
exit "$fail"
