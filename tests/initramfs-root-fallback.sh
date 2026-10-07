#!/bin/sh
# A local disk's root that does not come up falls back to the claimed image
# (#244, #246's proposed test).
#
# server1 on 11.56: the disk held the release by volume id, so /init booted
# it, and its root would not mount (`erofs: cannot find valid erofs
# superblock`); the boot stopped at a shell. Pinned here:
#   * once per boot, from a local disk with an appliance known and a name
#     that is not a guess: the engine is stopped, the claimed image boots,
#     and it is installed over the disk (INSTALL_OVER = the disk, not the
#     partition), with STORMBLOCK_RELAY_SYSTEM_HALF=1 so the engine lays the
#     system half again whatever the ids say (its data half is kept, #311);
#   * a claim is made when this boot has none yet;
#   * no fallback: a second time, a root that was not a local disk's (a
#     claimed image, a hook's decision), no appliance, a guessed name
#     (#249), iSCSI mode, an appliance that gives no image.
#
# Runs the real code: the block is extracted from the init script this repo
# generates, between its marker comments, so the test cannot drift.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN root fallback/,/# --- END root fallback/p' "$GEN" > "$WORK/fallback.sh"
[ -s "$WORK/fallback.sh" ] || { echo "FAIL: could not extract the root fallback block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok: $1"
    else
        echo "  FAIL: $1"
        echo "    expected: $2"
        echo "    got:      $3"
        fail=1
    fi
}

# run VAR=value... -> "rc|SLAB|INSTALL_OVER|RELAY|launches|claims|engine" and
# the block's output in $WORK/out
run() {
    (
        BOOT_MODE=local
        LOCAL_ROOT_SLAB=/dev/sda2
        BOOTHOST=forge:9090
        BOOTTAG=server1
        CLAIMED="nvme-tcp://forge:4420/nqn.test:host:server1?nsid=1"
        SLAB=/dev/sda2
        ROOTDEV="$WORK/no-such-ublkb0"
        ENGINE_LOG="$WORK/engine.log"
        : > "$ENGINE_LOG"
        GUESS=""
        CLAIM_GIVES="nvme-tcp://forge:4420/nqn.test:host:server1?nsid=2"
        unset STORMBLOCK_RELAY_SYSTEM_HALF
        STORMBLOCK_RESUME_SOURCE=left-over
        export STORMBLOCK_RESUME_SOURCE
        for kv in "$@"; do eval "$kv"; done
        LAUNCHES=0
        CLAIMS=0
        identity_guessed() { [ -n "$GUESS" ]; }
        boothost_claim() {
            CLAIMS=$((CLAIMS + 1))
            [ -n "$CLAIM_GIVES" ] || return 1
            CLAIMED="$CLAIM_GIVES"
        }
        launch_local() {
            LAUNCHES=$((LAUNCHES + 1))
            sleep 300 &
        }
        # The engine that served the failed root, and its follower.
        sleep 300 &
        STORMBLOCK_PID=$!
        OLD_ENGINE=$STORMBLOCK_PID
        sleep 300 &
        FOLLOW_PID=$!
        . "$WORK/fallback.sh"
        rc=0
        root_fallback "${WHY:-root device would not mount}" > "$WORK/out" 2>&1 || rc=$?
        if [ -n "${TWICE:-}" ] && [ "$rc" = 0 ]; then
            root_fallback "again" >> "$WORK/out" 2>&1 || rc="0+$?"
        fi
        engine=alive
        pid_alive "$OLD_ENGINE" || engine=stopped
        kill "$OLD_ENGINE" "$STORMBLOCK_PID" "$FOLLOW_PID" 2>/dev/null || true
        echo "$rc|$SLAB|${INSTALL_OVER:-}|${STORMBLOCK_RELAY_SYSTEM_HALF:-}|$LAUNCHES|$CLAIMS|$engine|${STORMBLOCK_RESUME_SOURCE:-}"
    )
}

echo "root fallback (#244):"
C1="nvme-tcp://forge:4420/nqn.test:host:server1?nsid=1"
C2="nvme-tcp://forge:4420/nqn.test:host:server1?nsid=2"

r=$(run)
check "a held disk's root fails: the claimed image boots, installed over the disk" \
    "0|$C1|/dev/sda|1|1|0|stopped|" "$r"
grep -q "FALLING BACK" "$WORK/out" && echo "  ok: says FALLING BACK" || { echo "  FAIL: no FALLING BACK"; fail=1; }
grep -q "data half kept" "$WORK/out" && echo "  ok: says the data half is kept" || { echo "  FAIL: data half not said"; fail=1; }

r=$(run LOCAL_ROOT_SLAB=/dev/nvme0n1p2 SLAB=/dev/nvme0n1p2)
check "an NVMe partition: installed over its disk" "0|$C1|/dev/nvme0n1|1|1|0|stopped|" "$r"

r=$(run CLAIMED="")
check "no claim made yet: claims, then falls back" "0|$C2|/dev/sda|1|1|1|stopped|" "$r"

r=$(run CLAIMED="" CLAIM_GIVES="")
check "the appliance gives no image: no fallback" "1|/dev/sda2|||0|1|alive|left-over" "$r"

r=$(run TWICE=1)
check "only once per boot" "0+1|$C1|/dev/sda|1|1|0|stopped|" "$r"
grep -q "already the fallback" "$WORK/out" && echo "  ok: says so the second time" || { echo "  FAIL: second time silent"; fail=1; }

r=$(run BOOTHOST="")
check "no appliance: no fallback" "1|/dev/sda2|||0|0|alive|left-over" "$r"
grep -q "no appliance" "$WORK/out" && echo "  ok: says there is no appliance" || { echo "  FAIL: no-appliance silent"; fail=1; }

r=$(run LOCAL_ROOT_SLAB="" SLAB="$C1")
check "the root was not a local disk's (claimed image, hook): no fallback" "1|$C1|||0|0|alive|left-over" "$r"

r=$(run GUESS=1)
check "a guessed name (#249): no fallback" "1|/dev/sda2|||0|0|alive|left-over" "$r"
grep -q "NOT FALLING BACK" "$WORK/out" && echo "  ok: says why" || { echo "  FAIL: guess silent"; fail=1; }

r=$(run BOOT_MODE=iscsi)
check "iSCSI mode: no fallback" "1|/dev/sda2|||0|0|alive|left-over" "$r"

[ "$fail" = 0 ] && echo "ALL PASS" || { echo "FAILED"; exit 1; }
