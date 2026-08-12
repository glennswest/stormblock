#!/bin/bash
# ci-fstemplate-verify.sh — does a real Linux kernel accept what we mkfs? (#38)
#
# The unit tests prove the engine can read back its own superblock, which
# proves nothing about whether the *kernel* will mount it. This runs the whole
# path in front of e2fsck and mount:
#
#   template (formatted in the engine, pure Rust, no mkfs.ext4)
#     → seal → clone ×2 → export over iSCSI → real initiator
#     → e2fsck -fn → mount → write → umount → e2fsck -fn again
#
# The single most diagnostic check is the pair of clone UUIDs: if two clones of
# one template present the same filesystem UUID, mount-by-UUID and the blkid
# cache collide the moment both are attached to one host — the bug clone-time
# stamping exists to prevent (stormblockmk#12).

set -uo pipefail

FAILURES=0
GREEN='' RED='' YELLOW='' CYAN='' BOLD='' RESET=''
if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'
    CYAN='\033[0;36m'; BOLD='\033[1m'; RESET='\033[0m'
fi
ok()   { echo -e "  ${GREEN}OK${RESET}: $1"; }
bad()  { echo -e "  ${RED}FAIL${RESET}: $1"; FAILURES=$((FAILURES+1)); }
info() { echo "  .. $1"; }
hdr()  { echo; echo -e "${BOLD}${CYAN}── $1 ──${RESET}"; echo; }

IQN="iqn.2024.io.stormblock:fstemplate"
PORT=3260
MGMT=9095
API="http://127.0.0.1:$MGMT/api/v1"
W="$(mktemp -d)"
SB_PID=""
LOGGED_IN=0

cleanup() {
    for m in "$W"/mnt-*; do mountpoint -q "$m" 2>/dev/null && umount "$m" 2>/dev/null; done
    if [ "$LOGGED_IN" = "1" ]; then
        iscsiadm -m node -T "$IQN" -p 127.0.0.1:$PORT --logout >/dev/null 2>&1
        iscsiadm -m node -T "$IQN" -p 127.0.0.1:$PORT -o delete >/dev/null 2>&1
    fi
    if [ -n "$SB_PID" ] && kill -0 "$SB_PID" 2>/dev/null; then
        kill "$SB_PID" 2>/dev/null; wait "$SB_PID" 2>/dev/null
    fi
    rm -rf "$W" 2>/dev/null
}
trap cleanup EXIT

jqf() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval('d$1'))" 2>/dev/null; }

echo -e "${BOLD}StormBlock — preformatted filesystem templates (#38)${RESET}"
echo "kernel: $(uname -r)   date: $(date)"

# ── Preflight ───────────────────────────────────────────────────────────────

hdr "Preflight"
if [ "$(id -u)" != "0" ]; then echo "must run as root"; exit 2; fi
for c in iscsiadm e2fsck dumpe2fs blkid curl python3; do
    command -v "$c" >/dev/null 2>&1 || { echo "missing: $c"; exit 2; }
done
ok "tools present"
systemctl start iscsid 2>/dev/null || service iscsid start 2>/dev/null || true
sleep 1

if [ ! -x ./target/debug/stormblock ]; then
    info "building"
    cargo build --bin stormblock 2>&1 | tail -5
fi
[ -x ./target/debug/stormblock ] || { echo "no binary"; exit 2; }

# ── Start the engine ────────────────────────────────────────────────────────

hdr "Start engine"
mkdir -p "$W/data"
truncate -s 4G "$W/d1.img"
truncate -s 4G "$W/d2.img"
cat > "$W/stormblock.toml" <<EOF
[management]
listen_addr = "127.0.0.1:$MGMT"
data_dir = "$W/data"
node_name = "ci-fstemplate"
EOF

# A RAID array plus one seed volume is what registers the slab the templates
# then allocate from.
RUST_LOG=stormblock=info ./target/debug/stormblock \
    --config "$W/stormblock.toml" \
    --device "$W/d1.img" --device "$W/d2.img" \
    --raid raid1 --volume seed:16M \
    --data-dir "$W/data" --no-nvmeof \
    --iscsi-addr "127.0.0.1:$PORT" --iscsi-target-name "$IQN" \
    > "$W/engine.log" 2>&1 &
SB_PID=$!

UP=0
for _ in $(seq 1 40); do
    curl -s -m 2 "$API/drives" >/dev/null 2>&1 && { UP=1; break; }
    kill -0 "$SB_PID" 2>/dev/null || break
    sleep 0.5
done
[ "$UP" = "1" ] || { bad "engine did not start"; tail -30 "$W/engine.log"; exit 1; }
ok "engine up"

# ── Build templates in the engine (no mkfs.ext4 anywhere) ───────────────────

hdr "mkfs once — templates formatted by the engine itself"

make_template() {  # name journal 64bit
    local name="$1" journal="$2" wide="${3:-false}" t0 t1
    t0=$(date +%s%N)
    local body
    body=$(curl -s -m 120 -X POST "$API/fstemplates" -H 'Content-Type: application/json' \
        -d "{\"name\":\"$name\",\"size\":\"256M\",\"journal\":$journal,\"64bit\":$wide,\"label\":\"$name\"}")
    t1=$(date +%s%N)
    local state
    state=$(echo "$body" | jqf "['template']['state']")
    if [ "$state" != "ready" ]; then
        bad "$name did not seal: $body"
        return 1
    fi
    ok "$name formatted+sealed in $(( (t1-t0)/1000000 )) ms (journal=$journal 64bit=$wide)"
    echo "$body" | jqf "['template']['fs_uuid']"
}

TPL_UUID_PLAIN=$(make_template "ext4-nojournal-256m" false)
TPL_UUID_JNL=$(make_template "ext4-journal-256m" true)
make_template "ext4-64bit-256m" true true >/dev/null

# ── Clone forever ───────────────────────────────────────────────────────────

hdr "clone forever — CoW clones with fresh identity"

clone() {  # template name
    local body t0 t1
    t0=$(date +%s%N)
    body=$(curl -s -m 60 -X POST "$API/fstemplates/$1/clone" -H 'Content-Type: application/json' \
        -d "{\"name\":\"$2\"}")
    t1=$(date +%s%N)
    local vol
    vol=$(echo "$body" | jqf "['volume_id']")
    if [ -z "$vol" ] || [ "$vol" = "None" ]; then
        bad "clone $2 failed: $body"
        return 1
    fi
    info "$2: volume $vol in $(( (t1-t0)/1000000 )) ms, fs_uuid=$(echo "$body" | jqf "['fs_uuid']")"
    echo "$vol"
}

VOL_A=$(clone "ext4-nojournal-256m" "clone-a")
VOL_B=$(clone "ext4-nojournal-256m" "clone-b")
VOL_J=$(clone "ext4-journal-256m" "clone-j")
VOL_W=$(clone "ext4-64bit-256m" "clone-w")
[ -n "$VOL_A" ] && [ -n "$VOL_B" ] && [ -n "$VOL_J" ] && [ -n "$VOL_W" ] || { bad "cloning failed"; exit 1; }
ok "4 clones minted"

# ── Export and attach with a real initiator ─────────────────────────────────

hdr "Export over iSCSI"
declare -A LUN
for pair in "A:$VOL_A" "B:$VOL_B" "J:$VOL_J" "W:$VOL_W"; do
    tag="${pair%%:*}"; vol="${pair#*:}"
    body=$(curl -s -m 30 -X POST "$API/exports" -H 'Content-Type: application/json' \
        -d "{\"volume_id\":\"$vol\",\"protocol\":\"iscsi\"}")
    lun=$(echo "$body" | jqf "['lun_id']")
    if [ -z "$lun" ] || [ "$lun" = "None" ]; then
        bad "export $tag failed: $body"; exit 1
    fi
    LUN[$tag]=$lun
    info "clone $tag → LUN $lun"
done
ok "exports created"

iscsiadm -m discovery -t sendtargets -p 127.0.0.1:$PORT >"$W/disc.log" 2>&1 || \
    info "discovery returned non-zero (login is what matters)"
if iscsiadm -m node -T "$IQN" -p 127.0.0.1:$PORT --login >"$W/login.log" 2>&1; then
    LOGGED_IN=1; ok "login"
else
    bad "login failed"; cat "$W/login.log"; exit 1
fi

declare -A DEV
for tag in A B J W; do
    d=""
    for _ in $(seq 1 30); do
        d=$(ls /dev/disk/by-path/*"$IQN"*lun-"${LUN[$tag]}" 2>/dev/null | head -1)
        [ -n "$d" ] && { d=$(readlink -f "$d"); break; }
        sleep 0.5
    done
    [ -n "$d" ] || { bad "no device for clone $tag (lun ${LUN[$tag]})"; exit 1; }
    DEV[$tag]=$d
    info "clone $tag → ${DEV[$tag]}"
done
ok "all clones visible to the kernel"

# ── THE diagnostic: identity ────────────────────────────────────────────────

hdr "Filesystem identity (stormblockmk#12)"
UUID_A=$(blkid -s UUID -o value "${DEV[A]}" 2>/dev/null)
UUID_B=$(blkid -s UUID -o value "${DEV[B]}" 2>/dev/null)
LABEL_A=$(blkid -s LABEL -o value "${DEV[A]}" 2>/dev/null)
info "clone A: UUID=$UUID_A LABEL=$LABEL_A"
info "clone B: UUID=$UUID_B"
info "template: $TPL_UUID_PLAIN"

[ -n "$UUID_A" ] && ok "blkid recognises the filesystem" || bad "blkid saw nothing — the kernel does not recognise this as ext4"
if [ -n "$UUID_A" ] && [ "$UUID_A" != "$UUID_B" ]; then
    ok "two clones of one template have distinct UUIDs"
else
    bad "clones share UUID $UUID_A — clone-time stamping is not working"
fi
if [ "$UUID_A" != "$TPL_UUID_PLAIN" ] && [ "$UUID_B" != "$TPL_UUID_PLAIN" ]; then
    ok "neither clone inherited the template's UUID"
else
    bad "a clone kept the template's UUID"
fi
[ "$LABEL_A" = "ext4-nojournal-256m" ] && ok "label survives cloning" || bad "label lost: '$LABEL_A'"

# ── e2fsck: is the on-disk format actually correct? ─────────────────────────

hdr "e2fsck — full check of a filesystem this engine wrote"
for tag in A J W; do
    if e2fsck -fn "${DEV[$tag]}" >"$W/fsck-$tag.log" 2>&1; then
        ok "clone $tag passes e2fsck -fn clean"
    else
        bad "clone $tag: e2fsck reported problems (exit $?)"
        sed -n '1,25p' "$W/fsck-$tag.log"
    fi
done

info "features (clone A):"
dumpe2fs -h "${DEV[A]}" 2>/dev/null | grep -E "Filesystem features|Filesystem state|Inode count|Block count|Free blocks" | sed 's/^/     /'
info "features (clone J):"
dumpe2fs -h "${DEV[J]}" 2>/dev/null | grep -E "Filesystem features|Journal|Filesystem state" | sed 's/^/     /'

if dumpe2fs -h "${DEV[J]}" 2>/dev/null | grep -q "has_journal"; then
    ok "journalled variant carries has_journal"
else
    bad "journalled template has no journal"
fi
if dumpe2fs -h "${DEV[A]}" 2>/dev/null | grep "Filesystem features" | grep -q "has_journal"; then
    bad "the journal-less template has a journal after all"
else
    ok "journal-less variant has none — RouterOS can mount it read-write"
fi
if dumpe2fs -h "${DEV[A]}" 2>/dev/null | grep "Filesystem features" | grep -qE "metadata_csum|64bit"; then
    bad "conservative feature set broken (metadata_csum/64bit present by default)"
else
    ok "conservative feature set held by default"
fi
info "features (clone W, 64bit template):"
dumpe2fs -h "${DEV[W]}" 2>/dev/null | grep -E "Filesystem features|Group descriptor size|Filesystem state" | sed 's/^/     /'
if dumpe2fs -h "${DEV[W]}" 2>/dev/null | grep "Filesystem features" | grep -q "64bit"; then
    ok "64bit variant carries the feature"
else
    bad "64bit template came out 32-bit"
fi
if dumpe2fs -h "${DEV[W]}" 2>/dev/null | grep "Filesystem features" | grep -q "metadata_csum"; then
    bad "64bit dragged metadata_csum in — the UUID stamp is no longer a plain patch"
else
    ok "64bit did not pull in metadata_csum"
fi

# ── Mount: the thing consumers actually do ──────────────────────────────────

hdr "Mount read-write, both clones at once"
for tag in A B J W; do
    mkdir -p "$W/mnt-$tag"
done

mount_check() {  # tag
    local tag="$1" dev="${DEV[$tag]}" mnt="$W/mnt-$tag"
    if ! mount "$dev" "$mnt" 2>"$W/mount-$tag.log"; then
        bad "clone $tag failed to mount: $(cat "$W/mount-$tag.log")"
        return 1
    fi
    # Read-only would be the stormblock-registry#10 symptom.
    if grep -qE " $mnt .*\bro\b" /proc/mounts; then
        bad "clone $tag mounted READ-ONLY — the seal guard let a dirty superblock through"
        return 1
    fi
    ok "clone $tag mounted read-write"
    echo "storm-$tag" > "$mnt/hello" 2>/dev/null || { bad "clone $tag: write failed"; return 1; }
    dd if=/dev/urandom of="$mnt/blob" bs=1M count=32 status=none 2>/dev/null || \
        { bad "clone $tag: bulk write failed"; return 1; }
    sync
    [ "$(cat "$mnt/hello")" = "storm-$tag" ] && ok "clone $tag: read back what was written" \
        || bad "clone $tag: content mismatch"
    [ -d "$mnt/lost+found" ] && ok "clone $tag: lost+found present" || bad "clone $tag: no lost+found"
}

for tag in A B J W; do mount_check "$tag"; done

# Divergence: a write into one clone must not appear in its sibling.
if [ -e "$W/mnt-B/hello" ] && [ "$(cat "$W/mnt-B/hello")" = "storm-B" ]; then
    ok "clones diverge — B kept its own content"
else
    bad "clone B saw the wrong content"
fi

# What the kernel logged while writing. If ext4 rejected anything about the
# on-disk layout it says so here and nowhere else — this is the diagnostic
# that separates "our format is wrong" from "the consumer is fussy" (#39).
hdr "Kernel ext4 log"
DMESG=$(dmesg -T 2>/dev/null | grep -iE "EXT4-fs|ext4_" | tail -25)
if [ -n "$DMESG" ]; then
    echo "$DMESG" | sed 's/^/     /'
    if echo "$DMESG" | grep -qiE "error|corrupt|invalid|remount|read-only"; then
        bad "the kernel logged an ext4 complaint against a filesystem we wrote"
    else
        ok "no ext4 errors logged"
    fi
else
    info "no ext4 lines in dmesg (kernel log unreadable in this container?)"
fi

hdr "Unmount and re-check"
for tag in A B J W; do
    umount "$W/mnt-$tag" 2>/dev/null || bad "clone $tag would not unmount"
done
sync
for tag in A J W; do
    if e2fsck -fn "${DEV[$tag]}" >"$W/fsck2-$tag.log" 2>&1; then
        ok "clone $tag still clean after a mount/write/unmount cycle"
    else
        bad "clone $tag: e2fsck problems after use"
        sed -n '1,25p' "$W/fsck2-$tag.log"
    fi
done

# ── Cost ────────────────────────────────────────────────────────────────────

hdr "Cost"
curl -s -m 10 "$API/fstemplates" | python3 -c "
import json,sys
for t in json.load(sys.stdin)['items']:
    print('  .. %-22s %s  clones=%d  journal=%s' % (t['name'], t['state'], t['clones'], t['journal']))
" 2>/dev/null
curl -s -m 10 "http://127.0.0.1:$MGMT/metrics" 2>/dev/null \
    | grep -E "^stormblock_slab_(allocated|free)_bytes_total" | sed 's/^/  .. /'

hdr "Verdict"
echo
if [ "$FAILURES" -eq 0 ]; then
    echo -e "  ${GREEN}${BOLD}All checks passed — the kernel mounts what this engine formats${RESET}"
    exit 0
else
    echo -e "  ${RED}${BOLD}$FAILURES check(s) failed${RESET}"
    echo
    echo "--- engine log tail ---"
    tail -25 "$W/engine.log"
    exit 1
fi
