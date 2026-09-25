#!/usr/bin/env bash
# ci-claim-timing.sh — what a PVC claim costs through the served engine (#137).
#
# `examples/claim_timing.rs` times each step of a mint in the library; this
# times what a caller sees over HTTP, on the binary a node runs, including
# the export:
#
#   - clone  POST /api/v1/fstemplates/{id}/clone  — a mint, nothing standing by
#   - claim  POST /api/v1/fstemplates/{id}/claim  — what a PVC does
#   - export POST /api/v1/volumes/{id}/attach {"transport":"nvme-tcp"}
#
# (The host's own `nvme connect` is the kernel's, needs root, and is not what
# #137 is about.)
#
#   sc-build 'cargo build --locked --release && ./ci-claim-timing.sh'
set -euo pipefail

BIN=${STORMBLOCK_BIN:-${CARGO_TARGET_DIR:-target}/release/stormblock}
W=${WORK:-$PWD/tmp/claim-timing}
MGMT=${MGMT:-127.0.0.1:9196}
NVME=${NVME:-127.0.0.1:4436}
N=${N:-9}
SIZES=${SIZES:-64M 1G 10G}
API="http://$MGMT/api/v1"

fail() { printf '\nFAIL: %s\n' "$*" >&2; [ -f "$W/engine.log" ] && tail -20 "$W/engine.log" >&2; exit 1; }
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }
ENGINE=
trap '[ -n "$ENGINE" ] && kill "$ENGINE" 2>/dev/null; wait 2>/dev/null || true' EXIT

[ -x "$BIN" ] || fail "no binary at $BIN"
rm -rf "$W"; mkdir -p "$W/data"
truncate -s 32G "$W/d1.img"
truncate -s 32G "$W/d2.img"
cat > "$W/stormblock.toml" <<EOF
[management]
listen_addr = "$MGMT"
data_dir = "$W/data"
node_name = "ci-claim"
EOF
RUST_LOG=stormblock=warn "$BIN" --config "$W/stormblock.toml" --device "$W/d1.img" --device "$W/d2.img" --raid raid1 --volume seed:16M \
    --data-dir "$W/data" --no-iscsi --nvmeof-addr "$NVME" --nvmeof-nqn nqn.2026-09.lo.test:claim \
    >"$W/engine.log" 2>&1 &
ENGINE=$!
for _ in $(seq 1 600); do
    curl -s -o /dev/null "$API/health" && break
    kill -0 "$ENGINE" 2>/dev/null || fail "engine exited"
    sleep 0.1
done
TOKEN=$(cat "$W/data/api_token")
H=(-H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json')

# POST, print "<ms> <body>".
timed_post() {
    curl -s -w '\n%{time_total}' -X POST "${H[@]}" -d "$2" "$1" | python3 -c '
import sys
lines = sys.stdin.read().rsplit("\n", 1)
print("%.2f" % (float(lines[1]) * 1000), lines[0])'
}
median() { python3 -c "import sys,statistics; v=sorted(float(x) for x in sys.argv[1:]); print('median %8.2f ms  (min %8.2f, max %8.2f)' % (statistics.median(v), v[0], v[-1]))" "$@"; }

echo "claim timing over HTTP — $N each, release build"
for size in $SIZES; do
    body=$(curl -s -m 300 -X POST "${H[@]}" "$API/fstemplates" -d "{\"name\":\"pvc-$size\",\"size\":\"$size\"}")
    [ "$(j 'd["template"]["state"]' <<<"$body")" = ready ] || fail "template $size: $body"
    TID=$(j 'd["template"]["id"]' <<<"$body")
    sleep 1   # let any post-seal work settle before measuring
    clone=() claim=() export=()
    for i in $(seq 1 "$N"); do
        read -r ms out < <(timed_post "$API/fstemplates/$TID/clone" "{\"name\":\"c-$size-$i\"}")
        clone+=("$ms")
        read -r ms out < <(timed_post "$API/fstemplates/$TID/claim" '{}')
        claim+=("$ms")
        VID=$(j 'd.get("volume_id") or d.get("id") or d["volume"]["id"]' <<<"$out")
        read -r ms out < <(timed_post "$API/volumes/$VID/attach" '{"transport":"nvme-tcp"}')
        export+=("$ms")
        grep -q nqn <<<"$out" || fail "attach did not export: $out"
    done
    echo "== $size"
    echo "  clone (mint)   $(median "${clone[@]}")"
    echo "  claim          $(median "${claim[@]}")"
    echo "  export         $(median "${export[@]}")"
done
echo
echo "volumes on the node that nothing asked for (standby):"
curl -s "${H[@]}" "$API/volumes" | python3 -c '
import json,sys
v=[x["name"] for x in json.load(sys.stdin)["items"] if x["name"].startswith("standby-")]
print(" ", len(v), v[:6])'
