#!/usr/bin/env bash
# ci-xfs-verify.sh — XFS alongside ext4, judged by the real xfsprogs (#147).
#
#   1. An XFS blank built by the engine's template lifecycle (mkfs-xfs on a
#      thin volume) and two claims of it: `xfs_repair -n` on each, `blkid`
#      says xfs with the UUID the engine recorded, the two claims and the
#      blank all differ, and each claim's metadata UUID is still the blank's
#      (`xfs_db`), which is what `xfs_admin -U` leaves.
#   2. A real Rocky Linux 9 cloud image (qcow2, GPT, XFS root) imported through
#      the served engine's API: the import finds the XFS root, names the OS
#      from /etc/os-release, and walks the tree. That walk is compared with
#      the inode count `xfs_db` gives for the same partition, and the
#      partition passes `xfs_repair -n`.
#
# No root: images are checked as files (`xfs_repair -f`), never mounted.
#
#   sc-build 'cargo build --locked --release --example xfs_verify && cargo build --locked --release && ./ci-xfs-verify.sh'
set -euo pipefail

T=${CARGO_TARGET_DIR:-target}/release
BIN=${STORMBLOCK_BIN:-$T/stormblock}
EX=$T/examples/xfs_verify
W=${WORK:-$PWD/tmp/xfs-verify}
MGMT=${MGMT:-127.0.0.1:9197}
API="http://$MGMT/api/v1"
IMAGE_URL=${IMAGE_URL:-https://dl.rockylinux.org/pub/rocky/9/images/x86_64/Rocky-9-GenericCloud-Base.latest.x86_64.qcow2}

fail() { printf '\nFAIL: %s\n' "$*" >&2; [ -f "$W/engine.log" ] && tail -20 "$W/engine.log" >&2; exit 1; }
say() { printf '\n== %s\n' "$*"; }
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }
ENGINE=
trap '[ -n "$ENGINE" ] && kill -9 "$ENGINE" 2>/dev/null; wait 2>/dev/null || true' EXIT

for t in xfs_repair xfs_db blkid qemu-img; do command -v $t >/dev/null || fail "$t is not installed"; done
[ -x "$EX" ] || fail "no example at $EX"
[ -x "$BIN" ] || fail "no binary at $BIN"
rm -rf "$W"; mkdir -p "$W"

say "1. an XFS blank and two claims, by the engine"
"$EX" "$W/blank" 2048 | tee "$W/uuids.txt"
declare -A U
while read -r name _ uuid _; do U[$name]=${uuid#uuid=}; done < "$W/uuids.txt"
for n in blank claim-a claim-b; do
    img="$W/blank/$n.img"
    xfs_repair -n -f "$img" >"$W/$n.repair" 2>&1 || { cat "$W/$n.repair"; fail "xfs_repair -n: $n"; }
    eval "$(blkid -p -o export "$img" | grep -E '^(TYPE|UUID|LABEL)=')"
    [ "$TYPE" = xfs ] || fail "$n: blkid says $TYPE"
    [ "$UUID" = "${U[$n]}" ] || fail "$n: blkid UUID $UUID, the engine recorded ${U[$n]}"
    meta=$(xfs_db -r -f -c "sb 0" -c "p meta_uuid" "$img" 2>/dev/null | awk '{print $3}')
    echo "  $n: xfs_repair -n clean; blkid TYPE=$TYPE UUID=$UUID LABEL=${LABEL:-}; meta_uuid=${meta:-(none)}"
    if [ "$n" != blank ]; then
        [ "$meta" = "${U[blank]}" ] || fail "$n: metadata UUID $meta, want the blank's ${U[blank]}"
        [ "$LABEL" = "$n" ] || fail "$n: label $LABEL"
    fi
    unset TYPE UUID LABEL
done
[ "${U[claim-a]}" != "${U[claim-b]}" ] && [ "${U[claim-a]}" != "${U[blank]}" ] && [ "${U[claim-b]}" != "${U[blank]}" ] \
    || fail "UUIDs collide: ${U[*]}"
echo "  three distinct UUIDs"

say "2. a Rocky 9 cloud image, imported through the engine"
curl -fsSL --retry 3 -o "$W/rocky.qcow2" "$IMAGE_URL" || fail "download $IMAGE_URL"
ls -l "$W/rocky.qcow2" | awk '{print "  downloaded", $5, "bytes"}'
mkdir -p "$W/data"
truncate -s 40G "$W/slab.img"
"$BIN" slab format "$W/slab.img" --role data >/dev/null 2>&1 || "$BIN" slab format "$W/slab.img" >/dev/null
cat > "$W/stormblock.toml" <<EOF
[[drives]]
path = "$W/slab.img"

[management]
listen_addr = "$MGMT"
data_dir = "$W/data"
node_name = "ci-xfs"
EOF
RUST_LOG=stormblock=info "$BIN" --config "$W/stormblock.toml" --data-dir "$W/data" --no-iscsi >>"$W/engine.log" 2>&1 &
ENGINE=$!
for _ in $(seq 1 600); do
    curl -s -o /dev/null "$API/health" && break
    kill -0 "$ENGINE" 2>/dev/null || fail "engine exited"
    sleep 0.1
done
H=(-H "Authorization: Bearer $(cat "$W/data/api_token")" -H 'Content-Type: application/json')
t0=$SECONDS
id=$(curl -s -X POST "${H[@]}" "$API/volumes/import" -d "{\"name\":\"rocky9\",\"file\":\"$W/rocky.qcow2\"}" | j 'd["id"]')
for _ in $(seq 1 1800); do
    st=$(curl -s "${H[@]}" "$API/volumes/import/$id")
    state=$(j 'd["state"]' <<<"$st")
    case $state in done|failed) break ;; esac
    sleep 1
done
echo "$st" | python3 -m json.tool > "$W/import.json"
[ "$state" = done ] || { cat "$W/import.json"; fail "import is $state"; }
echo "  imported in $((SECONDS - t0)) s"
read -r part off bytes entries os < <(python3 - "$W/import.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
print("  disk:", d.get("fs", {}).get("kind"), "written", d["written_bytes"], file=sys.stderr)
for f in d["filesystems"]:
    print(f"  partition {f.get('partition')}: {f['kind']} {f.get('label','')} os={f.get('os')} walked={f.get('walked')} error={f.get('error')}", file=sys.stderr)
roots = [f for f in d["filesystems"] if f["kind"] == "xfs" and f.get("os")]
assert roots, "no XFS root with an os-release was found"
r = roots[0]
assert "Rocky" in r["os"], r["os"]
assert not r.get("error"), r
print(r["partition"], r["offset"], r["bytes"], r["walked"]["entries"], r["os"].replace(" ", "_"))
PY
) || fail "the import did not find a readable XFS root"
echo "  root: partition $part, ${os//_/ }, $entries entries walked"

say "   ...and the same partition, judged by xfsprogs"
qemu-img convert -O raw "$W/rocky.qcow2" "$W/rocky.raw"
dd if="$W/rocky.raw" of="$W/root.img" bs=1M iflag=skip_bytes,count_bytes skip="$off" count="$bytes" conv=sparse status=none
xfs_repair -n -f "$W/root.img" >"$W/root.repair" 2>&1 || { tail "$W/root.repair"; fail "xfs_repair -n on the Rocky root"; }
counts=$(xfs_db -r -f -c "sb 0" -c "p icount" -c "p ifree" "$W/root.img")
icount=$(awk '/^icount/{print $3}' <<<"$counts"); ifree=$(awk '/^ifree/{print $3}' <<<"$counts")
used=$((icount - ifree))
echo "  xfs_repair -n clean; xfs_db: $used inodes in use; the engine's walk: $entries entries + the root"
# Every inode in use is reachable from the root, except the few the
# filesystem keeps for itself (realtime bitmap and summary, quotas).
[ $((entries + 1 + 4)) -ge "$used" ] || fail "the walk saw $entries entries but $used inodes are in use"
say "PASS"
