#!/usr/bin/env bash
# ci-template-resume.sh — a 1 TiB class blank on the served engine (#141).
#
# The blank rustkube-node mints for a 600Gi claim is the 1 TiB class. On the
# R230 it sat in `awaiting_format` for good: the create was persisted, then
# abandoned mid-format, and nothing ever finished it. This drives the binary a
# node runs through both ways that happens:
#
#   1. the caller gives up (curl --max-time 2) — the blank is still made
#      ready, because the create runs on a task of its own;
#   2. the engine is killed (-9) mid-format — on restart it finishes the job,
#      discarding what the partial format wrote first;
#
# and checks the result is a clean filesystem the size of the class.
#
#   sc-build 'cargo build --locked --release && ./ci-template-resume.sh'
set -euo pipefail

BIN=${STORMBLOCK_BIN:-${CARGO_TARGET_DIR:-target}/release/stormblock}
W=${WORK:-$PWD/tmp/template-resume}
MGMT=${MGMT:-127.0.0.1:9195}
API="http://$MGMT/api/v1"
SIZE=${SIZE:-1T}

fail() { printf '\nFAIL: %s\n' "$*" >&2; [ -f "$W/engine.log" ] && tail -20 "$W/engine.log" >&2; exit 1; }
say() { printf '\n== %s\n' "$*"; }
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }
ENGINE=
trap '[ -n "$ENGINE" ] && kill -9 "$ENGINE" 2>/dev/null; wait 2>/dev/null || true' EXIT

[ -x "$BIN" ] || fail "no binary at $BIN"
rm -rf "$W"; mkdir -p "$W/data"
truncate -s 1100G "$W/d1.img"
truncate -s 1100G "$W/d2.img"
cat > "$W/stormblock.toml" <<EOF
[management]
listen_addr = "$MGMT"
data_dir = "$W/data"
node_name = "ci-resume"
EOF
start() {
    RUST_LOG=stormblock=info "$BIN" --config "$W/stormblock.toml" --device "$W/d1.img" --device "$W/d2.img" \
        --raid raid1 --volume seed:16M --data-dir "$W/data" --no-iscsi --no-nvmeof >>"$W/engine.log" 2>&1 &
    ENGINE=$!
    for _ in $(seq 1 1200); do
        curl -s -o /dev/null "$API/health" && break
        kill -0 "$ENGINE" 2>/dev/null || fail "engine exited"
        sleep 0.1
    done
    H=(-H "Authorization: Bearer $(cat "$W/data/api_token")" -H 'Content-Type: application/json')
}
state_of() { curl -s "${H[@]}" "$API/fstemplates/$1" | j 'd.get("state") or d.get("template",{}).get("state")' 2>/dev/null || echo none; }
wait_ready() {
    local t0=$SECONDS
    for _ in $(seq 1 1200); do
        [ "$(state_of "$1")" = ready ] && { echo "  $1 ready after $((SECONDS - t0)) s"; return 0; }
        sleep 0.5
    done
    fail "$1 is $(state_of "$1") after 10 minutes"
}

say "an engine; a $SIZE blank whose caller gives up after 2 s"
start
t0=$SECONDS
curl -s --max-time 2 -X POST "${H[@]}" "$API/fstemplates" -d "{\"name\":\"pvc-ext4j-a\",\"size\":\"$SIZE\"}" \
    >/dev/null && echo "  (answered within 2 s)" || echo "  caller gave up after $((SECONDS - t0)) s"
wait_ready pvc-ext4j-a

say "a second $SIZE blank, and the engine killed mid-format"
curl -s --max-time 1 -X POST "${H[@]}" "$API/fstemplates" -d "{\"name\":\"pvc-ext4j-b\",\"size\":\"$SIZE\"}" >/dev/null || true
sleep 1
echo "  state when killed: $(state_of pvc-ext4j-b)"
kill -9 "$ENGINE"; wait "$ENGINE" 2>/dev/null || true; ENGINE=
[ "$(python3 -c "import json;print([t['state'] for t in json.load(open('$W/data/fstemplates.json'))['templates'] if t['name']=='pvc-ext4j-b'][0])")" = awaiting_format ] \
    || fail "the kill did not land mid-format — nothing to resume"
echo "  persisted as awaiting_format"

say "restart: the engine finishes the format it was killed in"
start
wait_ready pvc-ext4j-b
grep -q "finishing a format the engine did not complete" "$W/engine.log" || fail "no resume in the log"

say "each is a clean filesystem on a volume the size of the class"
for n in pvc-ext4j-a pvc-ext4j-b; do
    body=$(curl -s "${H[@]}" "$API/fstemplates/$n")
    vid=$(j 'd.get("sealed_volume_id") or d["template"]["sealed_volume_id"]' <<<"$body")
    size=$(j 'd.get("size_bytes") or d["template"]["size_bytes"]' <<<"$body")
    vsize=$(curl -s "${H[@]}" "$API/volumes/$vid" | j 'd["virtual_size_bytes"]')
    fsck=$(curl -s -m 600 -X POST "${H[@]}" "$API/volumes/$vid/fsck")
    python3 - "$fsck" "$size" "$vsize" "$n" <<'PY' || fail "$n did not check out: $fsck"
import json, sys
r, size, vsize, name = json.loads(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
print(f"  {name}: clean={r['clean']} problems={len(r['problems'])} volume={vsize} class={size}")
assert r["clean"] and not r["problems"], r
assert vsize == size == 1 << 40, (vsize, size)
PY
done
say "PASS"
