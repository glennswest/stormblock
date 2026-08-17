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
# Progress goes to stderr: these are called from inside $( ) capture, and a
# progress line landing in the captured value is a bug that hides itself.
ok()   { echo -e "  ${GREEN}OK${RESET}: $1" >&2; }
bad()  { echo -e "  ${RED}FAIL${RESET}: $1" >&2; FAILURES=$((FAILURES+1)); }
info() { echo "  .. $1" >&2; }
hdr()  { echo >&2; echo -e "${BOLD}${CYAN}── $1 ──${RESET}" >&2; echo >&2; }

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

# Pull one field out of a JSON body: `jqf template state` for
# `.template.state`. The key path comes in as argv rather than being pasted
# into the program text — quoting it into an eval collides with the quotes
# already there and silently yields nothing.
jqf() {
    python3 -c '
import json, sys
d = json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d[k]
print("" if d is None else d)
' "$@" 2>/dev/null
}

echo -e "${BOLD}StormBlock — preformatted filesystem templates (#38)${RESET}"
echo "kernel: $(uname -r)   date: $(date)"

# Where the kernel log stood before this run, so the check below reads only
# what this run caused. Without it a previous run's failures are re-reported
# as if they were current.
DMESG_MARK=$(dmesg 2>/dev/null | wc -l)

# ── Preflight ───────────────────────────────────────────────────────────────

hdr "Preflight"
if [ "$(id -u)" != "0" ]; then echo "must run as root"; exit 2; fi
for c in iscsiadm e2fsck dumpe2fs blkid curl python3; do
    command -v "$c" >/dev/null 2>&1 || { echo "missing: $c"; exit 2; }
done
ok "tools present"
systemctl start iscsid 2>/dev/null || service iscsid start 2>/dev/null || true
sleep 1

# Always build. A stale binary from a previous run silently verifies the
# previous commit, which is worse than not running at all.
info "building"
cargo build --bin stormblock 2>&1 | tail -3 >&2
[ -x ./target/debug/stormblock ] || { echo "no binary"; exit 2; }
info "binary: $(date -r ./target/debug/stormblock '+%Y-%m-%d %H:%M:%S'), tree: $(git log --oneline -1)"

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

make_template() {  # name extra-json
    local name="$1" extra="${2:-}" t0 t1
    t0=$(date +%s%N)
    local body
    body=$(curl -s -m 120 -X POST "$API/fstemplates" -H 'Content-Type: application/json' \
        -d "{\"name\":\"$name\",\"size\":\"256M\",\"label\":\"$name\"$extra}")
    t1=$(date +%s%N)
    local state
    state=$(echo "$body" | jqf template state)
    if [ "$state" != "ready" ]; then
        bad "$name did not seal: $body"
        return 1
    fi
    ok "$name formatted+sealed in $(( (t1-t0)/1000000 )) ms$(echo "$extra" | sed 's/^,/ (/;s/$/)/')"
    echo "$body" | jqf template fs_uuid
}

# The default is what mke2fs -t ext4 writes, which is what RouterOS's own
# format-drive produces. The second is for a consumer that predates it.
TPL_UUID_JNL=$(make_template "ext4-256m")
TPL_UUID_PLAIN=$(make_template "ext4-plain-256m" ',"journal":false,"features":"^64bit,^metadata_csum"')

# A template that ships content. The engine writes these files in userspace
# through fio-ext4 — no mount, no loop device, no attach — so the kernel is the
# only thing that can say whether what it wrote is a filesystem or merely
# something fio-ext4 can read back. SEED_N names in one directory is the point:
# past one block a directory has to become a hash tree, and a wrong tree is
# structurally perfect and still unreadable.
SEED_N=400
python3 - "$SEED_N" > "$W/seeded.json" <<'PYEOF'
import json, sys
n = int(sys.argv[1])
files = [
    {"path": "/boot.toml", "contents": 'slab = "local"\nvolume = "root"\n'},
    {"path": "/etc/hostname", "contents": "seeded\n"},
    {"path": "/etc/sysconfig/network/deep/nested/file", "contents": "deep\n"},
    {"path": "/big", "contents": "x" * 200000},
]
files += [{"path": f"/many/entry-{i:04d}.conf", "contents": f"n={i}\n"} for i in range(n)]
json.dump({"name": "ext4-seeded-256m", "size": "256M", "label": "seeded", "files": files},
          sys.stdout)
PYEOF
SEED_T0=$(date +%s%N)
SEED_BODY=$(curl -s -m 180 -X POST "$API/fstemplates" -H 'Content-Type: application/json' \
    --data-binary "@$W/seeded.json")
SEED_T1=$(date +%s%N)
if [ "$(echo "$SEED_BODY" | jqf template state)" = "ready" ]; then
    ok "ext4-seeded-256m formatted, seeded with $((SEED_N + 4)) files and sealed in $(( (SEED_T1-SEED_T0)/1000000 )) ms"
else
    bad "the seeded template did not seal: $SEED_BODY"
fi

# Concurrency: the formatter takes &self and no lock is held across a format,
# so N templates should cost about what one does rather than N times as much.
hdr "Concurrent formats"
CSTART=$(date +%s%N)
PAR_PIDS=()
for i in 1 2 3 4; do
    curl -s -m 180 -X POST "$API/fstemplates" -H 'Content-Type: application/json' \
        -d "{\"name\":\"parallel-$i\",\"size\":\"256M\"}" > "$W/par-$i.json" &
    PAR_PIDS+=($!)
done
# Only these: a bare `wait` would also wait on the engine, which never exits.
wait "${PAR_PIDS[@]}"
CEND=$(date +%s%N)
PAR_MS=$(( (CEND-CSTART)/1000000 ))
PAR_OK=0
for i in 1 2 3 4; do
    [ "$(jqf template state < "$W/par-$i.json")" = "ready" ] && PAR_OK=$((PAR_OK+1))
done
if [ "$PAR_OK" = "4" ]; then
    ok "4 templates formatted concurrently in $PAR_MS ms (one alone: see above)"
else
    bad "only $PAR_OK of 4 concurrent formats sealed"
fi

# ── Clone forever ───────────────────────────────────────────────────────────

hdr "clone forever — CoW clones with fresh identity"

clone() {  # template name
    local body t0 t1
    t0=$(date +%s%N)
    body=$(curl -s -m 60 -X POST "$API/fstemplates/$1/clone" -H 'Content-Type: application/json' \
        -d "{\"name\":\"$2\"}")
    t1=$(date +%s%N)
    local vol
    vol=$(echo "$body" | jqf volume_id)
    if [ -z "$vol" ] || [ "$vol" = "None" ]; then
        bad "clone $2 failed: $body"
        return 1
    fi
    info "$2: volume $vol in $(( (t1-t0)/1000000 )) ms, fs_uuid=$(echo "$body" | jqf fs_uuid)"
    echo "$vol"
}

VOL_A=$(clone "ext4-256m" "clone-a")
VOL_B=$(clone "ext4-256m" "clone-b")
VOL_J=$(clone "ext4-plain-256m" "clone-j")
VOL_W=$(clone "ext4-256m" "clone-w")
VOL_S=$(clone "ext4-seeded-256m" "clone-s")
[ -n "$VOL_A" ] && [ -n "$VOL_B" ] && [ -n "$VOL_J" ] && [ -n "$VOL_W" ] && [ -n "$VOL_S" ] || { bad "cloning failed"; exit 1; }
ok "5 clones minted"

# ── Export and attach with a real initiator ─────────────────────────────────

hdr "Export over iSCSI"
declare -A LUN
for pair in "A:$VOL_A" "B:$VOL_B" "J:$VOL_J" "W:$VOL_W" "S:$VOL_S"; do
    tag="${pair%%:*}"; vol="${pair#*:}"
    body=$(curl -s -m 30 -X POST "$API/exports" -H 'Content-Type: application/json' \
        -d "{\"volume_id\":\"$vol\",\"protocol\":\"iscsi\"}")
    lun=$(echo "$body" | jqf lun_id)
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
for tag in A B J W S; do
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

# ── The engine's own check, before the kernel sees any of it ────────────────

hdr "Engine-side fsck"
for pair in "A:$VOL_A" "B:$VOL_B" "J:$VOL_J" "S:$VOL_S"; do
    tag="${pair%%:*}"; vol="${pair#*:}"
    body=$(curl -s -m 120 -X POST "$API/volumes/$vol/fsck")
    clean=$(echo "$body" | jqf clean)
    if [ "$clean" = "True" ]; then
        ok "clone $tag: engine fsck clean"
    else
        bad "clone $tag: engine fsck found problems: $(echo "$body" | jqf problems)"
    fi
done

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
[ "$LABEL_A" = "ext4-256m" ] && ok "label survives cloning" || bad "label lost: '$LABEL_A'"

# ── e2fsck: is the on-disk format actually correct? ─────────────────────────

hdr "e2fsck — full check of a filesystem this engine wrote"
for tag in A J W S; do
    if e2fsck -fn "${DEV[$tag]}" >"$W/fsck-$tag.log" 2>&1; then
        ok "clone $tag passes e2fsck -fn clean"
    else
        bad "clone $tag: e2fsck reported problems (exit $?)"
        sed -n '1,25p' "$W/fsck-$tag.log"
    fi
done

BS_A=$(dumpe2fs -h "${DEV[A]}" 2>/dev/null | awk -F: '/^Block size/{gsub(/ /,"",$2); print $2}')
SEC_A=$(cat "/sys/block/$(basename "${DEV[A]}")/queue/logical_block_size" 2>/dev/null || echo 0)
info "block size $BS_A on a $SEC_A-byte-sector LUN"
if [ "${BS_A:-0}" -ge "${SEC_A:-0}" ] 2>/dev/null; then
    ok "filesystem blocks are not smaller than the device sectors"
else
    bad "block size $BS_A under a $SEC_A-byte sector — the kernel will refuse to mount it"
fi
info "features (clone A):"
dumpe2fs -h "${DEV[A]}" 2>/dev/null | grep -E "Filesystem features|Filesystem state|Inode count|Block count|Free blocks" | sed 's/^/     /'
info "features (clone J):"
dumpe2fs -h "${DEV[J]}" 2>/dev/null | grep -E "Filesystem features|Journal|Filesystem state" | sed 's/^/     /'

# Clone J is the ^64bit,^metadata_csum,journal:false variant; the default (A)
# is the journalled one, checked in the feature loop below.
if dumpe2fs -h "${DEV[J]}" 2>/dev/null | grep -q "has_journal"; then
    bad "the journal-less variant has a journal after all"
else
    ok "journal-less variant has none"
fi
# The default template must carry the profile RouterOS's own format-drive
# produces (#39): journal, 64bit, flex_bg, metadata_csum — plus the seed that
# keeps a clone-time UUID stamp a single write.
FEAT_A=$(dumpe2fs -h "${DEV[A]}" 2>/dev/null | grep "Filesystem features")
for f in has_journal 64bit flex_bg metadata_csum metadata_csum_seed extent; do
    if echo "$FEAT_A" | grep -q "$f"; then
        ok "default template carries $f"
    else
        bad "default template is missing $f"
    fi
done

info "features (clone J, the ^64bit,^metadata_csum variant):"
dumpe2fs -h "${DEV[J]}" 2>/dev/null | grep -E "Filesystem features|Filesystem state" | sed 's/^/     /'
if dumpe2fs -h "${DEV[J]}" 2>/dev/null | grep "Filesystem features" | grep -qE "metadata_csum|64bit|has_journal"; then
    bad "the -O overrides did not take"
else
    ok "-O overrides took: no journal, no 64bit, no metadata_csum"
fi

# ── Mount: the thing consumers actually do ──────────────────────────────────

hdr "Mount read-write, both clones at once"
for tag in A B J W S; do
    mkdir -p "$W/mnt-$tag"
done

# Every seeded entry, contents and all: an index that points a name at the
# wrong leaf still lists every name, and each entry says which one it is.
seed_sweep() {  # when
    local when="$1" bad=0 first="" i f got
    for i in $(seq 0 $((SEED_N - 1))); do
        f=$(printf "%s/mnt-S/many/entry-%04d.conf" "$W" "$i")
        # NULs would otherwise be dropped with a warning and compare equal.
        got=$(head -c 32 "$f" 2>/dev/null | tr -d '\0')
        [ "$got" = "n=$i" ] || { bad=$((bad+1)); [ -z "$first" ] && first=$i; }
    done
    if [ "$bad" = "0" ]; then
        ok "clone S: every seeded entry gives back its own contents ($when)"
    else
        bad "clone S: $bad of $SEED_N entries read back wrong $when (first: entry-$(printf %04d "$first"))"
        od -An -c "$(printf "%s/mnt-S/many/entry-%04d.conf" "$W" "$first")" 2>/dev/null | head -3 | sed 's/^/     /'
    fi
}

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
    # Read the seeded content before this clone is written to, so a failure
    # after the writes can be told from one that was there on arrival.
    [ "$tag" = "S" ] && seed_sweep "on arrival, before any write"
    echo "storm-$tag" > "$mnt/hello" 2>/dev/null || { bad "clone $tag: write failed"; return 1; }
    dd if=/dev/urandom of="$mnt/blob" bs=1M count=32 status=none 2>/dev/null || \
        { bad "clone $tag: bulk write failed"; return 1; }
    sync
    [ "$(cat "$mnt/hello")" = "storm-$tag" ] && ok "clone $tag: read back what was written" \
        || bad "clone $tag: content mismatch"
    [ -d "$mnt/lost+found" ] && ok "clone $tag: lost+found present" || bad "clone $tag: no lost+found"
}

for tag in A B J W S; do mount_check "$tag"; done

# Divergence: a write into one clone must not appear in its sibling.
if [ -e "$W/mnt-B/hello" ] && [ "$(cat "$W/mnt-B/hello")" = "storm-B" ]; then
    ok "clones diverge — B kept its own content"
else
    bad "clone B saw the wrong content"
fi

# ── Seeded content, read by the kernel rather than by the writer ───────────

hdr "Seeded content — what fio-ext4 wrote, read back through ext4"

M="$W/mnt-S"
if [ -f "$M/boot.toml" ] && grep -q 'volume = "root"' "$M/boot.toml"; then
    ok "clone S: /boot.toml present with the right contents"
else
    bad "clone S: /boot.toml missing or wrong"
fi
[ "$(cat "$M/etc/hostname" 2>/dev/null)" = "seeded" ] \
    && ok "clone S: parent directories were created" \
    || bad "clone S: /etc/hostname missing"
[ -f "$M/etc/sysconfig/network/deep/nested/file" ] \
    && ok "clone S: a five-deep path resolves" \
    || bad "clone S: the deep path is not there"
BIG=$(stat -c %s "$M/big" 2>/dev/null || echo 0)
if [ "$BIG" = "200000" ] && [ "$(tr -d 'x' < "$M/big" | wc -c)" = "0" ]; then
    ok "clone S: a 200 KB multi-block file is intact ($BIG bytes)"
else
    bad "clone S: /big is $BIG bytes, expected 200000"
fi

# The directory that had to become a hash tree. Every name present is the
# check: an index that loses names still passes e2fsck if the leaves it does
# point at are well formed.
SEEN=$(ls -U "$M/many" 2>/dev/null | wc -l)
if [ "$SEEN" = "$SEED_N" ]; then
    ok "clone S: all $SEED_N names in the indexed directory are readable"
else
    bad "clone S: /many holds $SEEN names, expected $SEED_N"
fi
seed_sweep "after 32 MB was written into it"

if dumpe2fs -h "${DEV[S]}" 2>/dev/null | grep "Filesystem features" | grep -q dir_index; then
    ok "clone S: dir_index is set"
else
    bad "clone S: no dir_index feature"
fi
# debugfs reads the tree with the counts, limits and checksums it expects —
# the one check that is not ours marking our own homework.
if debugfs -R "htree_dump /many" "${DEV[S]}" >"$W/htree.log" 2>&1 && \
   grep -qiE "Number of entries|Indirect level|Entry #" "$W/htree.log"; then
    ok "clone S: debugfs reads the hash tree ($(grep -ciE '^ *Entry #' "$W/htree.log") index entries)"
elif grep -qi "not a hash-indexed" "$W/htree.log"; then
    bad "clone S: /many never became a hash tree — $SEED_N names should have forced one"
else
    bad "clone S: debugfs could not read the tree: $(tail -3 "$W/htree.log" | tr '\n' ' ')"
fi

# Writing into a seeded clone: the kernel now maintains the same tree.
if cp "$M/boot.toml" "$M/many/added-by-kernel.conf" 2>/dev/null && sync; then
    ok "clone S: the kernel adds a name to the tree the engine built"
else
    bad "clone S: the kernel would not write into the indexed directory"
fi

# What the kernel logged while writing. If ext4 rejected anything about the
# on-disk layout it says so here and nowhere else — this is the diagnostic
# that separates "our format is wrong" from "the consumer is fussy" (#39).
hdr "Kernel ext4 log"
DMESG=$(dmesg -T 2>/dev/null | tail -n +$((DMESG_MARK + 1)) | grep -iE "EXT4-fs|ext4_" | tail -25)
if [ -n "$DMESG" ]; then
    echo "$DMESG" | sed 's/^/     /'
    if echo "$DMESG" | grep -qiE "error|corrupt|invalid|remount|read-only|bad block size|cannot|unable|failed"; then
        bad "the kernel logged an ext4 complaint against a filesystem we wrote"
    else
        ok "no ext4 errors logged"
    fi
else
    info "no ext4 lines in dmesg (kernel log unreadable in this container?)"
fi

hdr "Unmount and re-check"
for tag in A B J W S; do
    umount "$W/mnt-$tag" 2>/dev/null || bad "clone $tag would not unmount"
done
sync
for tag in A J W S; do
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
