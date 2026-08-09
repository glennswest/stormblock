#!/bin/bash
# ci-iscsi-reclaim.sh — end-to-end proof for #25 (thin allocation must shrink).
#
# The bug was on the iSCSI path specifically: the target never advertised thin
# provisioning (VPD 0xB2 absent), so Linux left discard_max_bytes at 0 and
# issued no UNMAP at all; and even if it had, UNMAP's parameter list was never
# collected off the wire. NVMe DSM was already working, so testing over NVMe
# would prove nothing about the fix.
#
# So: real open-iscsi initiator → ext4 → fill → delete → fstrim, watching the
# engine's own slab accounting. The single most diagnostic check is
# discard_max_bytes on the block device: if that is 0, the VPD advertising is
# still wrong and nothing downstream can possibly reclaim.

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

IQN="iqn.2024.io.stormblock:cireclaim"
PORT=3260
MGMT=9090
W="$(mktemp -d)"
MNT="$W/mnt"
SB_PID=""
LOGGED_IN=0

cleanup() {
    mountpoint -q "$MNT" 2>/dev/null && umount "$MNT" 2>/dev/null
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

# Engine-side accounting, straight from /metrics.
slab_metric() {
    curl -s -m 10 "http://127.0.0.1:$MGMT/metrics" 2>/dev/null \
        | awk -v k="$1" '$1==k {print $2; exit}'
}
mb() { awk -v b="${1:-0}" 'BEGIN{printf "%.1f", b/1048576}'; }

echo -e "${BOLD}StormBlock — iSCSI thin reclaim verification (#25)${RESET}"
echo "kernel: $(uname -r)   date: $(date)"

# ── Preflight ───────────────────────────────────────────────────────────────

hdr "Preflight"
if [ "$(id -u)" != "0" ]; then echo "must run as root"; exit 2; fi
for c in iscsiadm mkfs.ext4 fstrim curl; do
    command -v "$c" >/dev/null 2>&1 || { echo "missing: $c"; exit 2; }
done
ok "tools present"
systemctl start iscsid 2>/dev/null || service iscsid start 2>/dev/null || true
sleep 1

# ── Start the target ────────────────────────────────────────────────────────

hdr "Start target (volume-backed LUN)"
mkdir -p "$W/data" "$MNT"
truncate -s 2G "$W/d1.img"
truncate -s 2G "$W/d2.img"
cat > "$W/stormblock.toml" <<EOF
[management]
listen_addr = "127.0.0.1:$MGMT"
data_dir = "$W/data"
node_name = "ci-node"
EOF

RUST_LOG=stormblock=info ./target/debug/stormblock \
    --config "$W/stormblock.toml" \
    --device "$W/d1.img" --device "$W/d2.img" \
    --raid raid1 --volume reclaim:1G \
    --data-dir "$W/data" --no-nvmeof \
    --iscsi-addr "127.0.0.1:$PORT" --iscsi-target-name "$IQN" \
    > "$W/target.log" 2>&1 &
SB_PID=$!

UP=0
for _ in $(seq 1 40); do
    curl -s -m 2 "http://127.0.0.1:$MGMT/api/v1/drives" >/dev/null 2>&1 && { UP=1; break; }
    kill -0 "$SB_PID" 2>/dev/null || break
    sleep 0.5
done
[ "$UP" = "1" ] || { bad "target did not start"; tail -30 "$W/target.log"; exit 1; }
ok "target up"

# ── Attach with the real initiator ──────────────────────────────────────────

hdr "iSCSI login"
iscsiadm -m discovery -t sendtargets -p 127.0.0.1:$PORT >"$W/disc.log" 2>&1 \
    && ok "discovery" || info "discovery returned non-zero (continuing; login is what matters)"
if iscsiadm -m node -T "$IQN" -p 127.0.0.1:$PORT --login >"$W/login.log" 2>&1; then
    LOGGED_IN=1; ok "login"
else
    bad "login failed"; cat "$W/login.log"; exit 1
fi

DEV=""
for _ in $(seq 1 30); do
    DEV=$(ls /dev/disk/by-path/*"$IQN"*lun-0 2>/dev/null | head -1)
    [ -n "$DEV" ] && { DEV=$(readlink -f "$DEV"); break; }
    sleep 0.5
done
[ -n "$DEV" ] || { bad "no block device appeared"; exit 1; }
ok "device: $DEV"
BASE=$(basename "$DEV")

# ── THE diagnostic: did the initiator enable discard at all? ────────────────

hdr "Thin-provisioning advertising (the #25 root cause)"
DMAX=$(cat "/sys/block/$BASE/queue/discard_max_bytes" 2>/dev/null || echo 0)
DGRAN=$(cat "/sys/block/$BASE/queue/discard_granularity" 2>/dev/null || echo 0)
info "discard_max_bytes=$DMAX  discard_granularity=$DGRAN"
if [ "${DMAX:-0}" -gt 0 ]; then
    ok "initiator enabled discard — VPD 0xB2 advertising works"
else
    bad "discard_max_bytes=0 — initiator will never issue UNMAP (VPD still wrong)"
fi
if [ "${DGRAN:-0}" -gt 0 ]; then
    ok "granularity advertised ($(mb "$DGRAN") MB)"
else
    info "granularity 0"
fi

# ── Fill, delete, trim ──────────────────────────────────────────────────────

hdr "Fill → delete → fstrim"
ALLOC0=$(slab_metric stormblock_slab_allocated_bytes_total)
FREE0=$(slab_metric stormblock_slab_free_bytes_total)
info "baseline: allocated=$(mb "$ALLOC0") MB free=$(mb "$FREE0") MB"

mkfs.ext4 -q -F "$DEV" >/dev/null 2>&1 && ok "mkfs.ext4" || bad "mkfs failed"
mount -o discard "$DEV" "$MNT" 2>/dev/null || mount "$DEV" "$MNT"
ok "mounted"

dd if=/dev/urandom of="$MNT/fill.bin" bs=1M count=300 status=none 2>/dev/null
sync
ALLOC1=$(slab_metric stormblock_slab_allocated_bytes_total)
info "after 300 MB write: allocated=$(mb "$ALLOC1") MB"
if awk -v a="$ALLOC1" -v b="$ALLOC0" 'BEGIN{exit !(a>b)}'; then
    ok "allocation grew by $(mb "$((ALLOC1-ALLOC0))") MB"
else
    bad "allocation did not grow — write path not reaching the slab"
fi

rm -f "$MNT/fill.bin"
sync
TRIM=$(fstrim -v "$MNT" 2>&1) && ok "fstrim: $TRIM" || bad "fstrim failed: $TRIM"
sync; sleep 3

ALLOC2=$(slab_metric stormblock_slab_allocated_bytes_total)
FREE2=$(slab_metric stormblock_slab_free_bytes_total)
info "after trim: allocated=$(mb "$ALLOC2") MB free=$(mb "$FREE2") MB"

hdr "Verdict"
RECLAIMED=$((ALLOC1 - ALLOC2))
echo "  allocated: $(mb "$ALLOC0") → $(mb "$ALLOC1") → $(mb "$ALLOC2") MB"
echo "  reclaimed by trim: $(mb "$RECLAIMED") MB"
if [ "$RECLAIMED" -gt 0 ] 2>/dev/null; then
    ok "RECLAIM VERIFIED — thin allocation shrank after fstrim"
else
    bad "allocation did not shrink — #25 not actually fixed on this path"
    echo "--- target log (UNMAP-related) ---"
    grep -iE "unmap|discard|0x42|write same" "$W/target.log" | tail -20
fi

echo
echo "--- target log tail ---"
tail -15 "$W/target.log"

echo
if [ "$FAILURES" -eq 0 ]; then
    echo -e "  ${GREEN}${BOLD}All checks passed${RESET}"; exit 0
else
    echo -e "  ${RED}${BOLD}$FAILURES check(s) failed${RESET}"; exit 1
fi
