#!/usr/bin/env bash
# ci-auth-verify.sh — the served engine is closed by default, and the boot
# claim is the one thing open (#107).
#
# The HTTP tests drive the router; this drives the binary a node runs, with a
# config that says nothing about auth:
#
#   - it mints a token into <data_dir>/api_token, mode 0600, and logs that it
#     requires one — no SECURITY line;
#   - /api/v1/health answers without a token and reports auth=required;
#   - a read, a re-point and a synonym create are 401 without the token;
#   - with the token: build a sealed golden, point boothost/default at it;
#   - WITHOUT a token: a new machine claims, gets a clone of its own sealed
#     golden (not the release), is pinned to the default, and a second claim
#     reuses the golden and mints a fresh clone;
#   - a restart reads the same token back.
#
#   sc-build 'cargo build --locked && ./ci-auth-verify.sh'
set -euo pipefail

BIN=${STORMBLOCK_BIN:-${CARGO_TARGET_DIR:-target}/debug/stormblock}
W=${WORK:-$PWD/tmp/auth-verify}
MGMT=${MGMT:-127.0.0.1:9197}
API="http://$MGMT/api/v1"

say() { printf '\n== %s\n' "$*"; }
fail() { printf '\nFAIL: %s\n' "$*" >&2; [ -f "$W/engine.log" ] && tail -20 "$W/engine.log" >&2; exit 1; }
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

ENGINE=
stop() {
    [ -n "$ENGINE" ] || return 0
    kill "$ENGINE" 2>/dev/null || true
    for _ in $(seq 1 50); do kill -0 "$ENGINE" 2>/dev/null || break; sleep 0.1; done
    kill -9 "$ENGINE" 2>/dev/null || true
    wait "$ENGINE" 2>/dev/null || true
    ENGINE=
}
trap stop EXIT

[ -x "$BIN" ] || fail "no binary at $BIN"
rm -rf "$W"; mkdir -p "$W/data"
truncate -s 1G "$W/d1.img"
truncate -s 1G "$W/d2.img"
# Nothing about auth in here: the default is what is being checked.
cat > "$W/stormblock.toml" <<EOF
[management]
listen_addr = "$MGMT"
data_dir = "$W/data"
node_name = "ci-auth"
EOF

start() {
    RUST_LOG=stormblock=info "$BIN" --config "$W/stormblock.toml" \
        --device "$W/d1.img" --device "$W/d2.img" --raid raid1 --volume seed:16M \
        --data-dir "$W/data" --no-nvmeof --no-iscsi >>"$W/engine.log" 2>&1 &
    ENGINE=$!
    for _ in $(seq 1 100); do
        [ "$(code "$API/health")" = 200 ] && return 0
        sleep 0.1
    done
    fail "engine did not come up"
}

say "an engine with no auth settings"
start
[ -f "$W/data/api_token" ] || fail "no token minted into the data dir"
[ "$(stat -c %a "$W/data/api_token")" = 600 ] || fail "token file is not mode 0600"
TOKEN=$(cat "$W/data/api_token")
grep -q "requires a bearer token, minted this boot" "$W/engine.log" || fail "the boot line does not say it is closed"
grep -q "UNAUTHENTICATED" "$W/engine.log" && fail "an engine with default settings says it is open"
echo "minted $W/data/api_token (0600); boot line says closed"

say "health is open and says auth is required"
curl -s "$API/health" | tee "$W/health.json"; echo
[ "$(j 'd["auth"]' < "$W/health.json")" = required ] || fail "health does not report auth=required"

say "without the token: 401"
for spec in "GET $API/volumes" "GET $API/synonyms" "GET http://$MGMT/metrics" \
            "PUT $API/synonyms/boothost/X1" "POST $API/synonyms" \
            "POST $API/synonyms/boothost/X1/rollback" "POST $API/synonyms/images/x/claim"; do
    set -- $spec
    c=$(code -X "$1" -H 'Content-Type: application/json' -d '{}' "$2")
    echo "  $1 ${2#http://$MGMT} -> $c"
    [ "$c" = 401 ] || fail "$1 $2 answered $c without a token"
done

AUTH=(-H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json')
say "with the token: a sealed release, and boothost/default"
curl -s -m 120 -X POST "$API/fstemplates" "${AUTH[@]}" -d '{"name":"release-a","size":"64M"}' > "$W/tpl.json"
[ "$(j 'd["template"]["state"]' < "$W/tpl.json")" = ready ] || fail "template did not seal: $(cat "$W/tpl.json")"
RELEASE=$(curl -s "$API/volumes" "${AUTH[@]}" | python3 -c "
import json,sys
v=[x for x in json.load(sys.stdin)['items'] if x['name']=='release-a' and x.get('sealed')]
print(v[0]['id'])")
echo "release-a = $RELEASE"
c=$(code -X POST "$API/synonyms" "${AUTH[@]}" -d "{\"namespace\":\"boothost\",\"name\":\"default\",\"volume\":\"$RELEASE\"}")
[ "$c" = 201 ] || fail "creating boothost/default answered $c"

say "WITHOUT a token: a new machine claims its boot image"
claim() { curl -s -X POST -H 'Content-Type: application/json' -d '{"namespace":"evil","name":"x","unsealed_ok":true}' \
    -w '\n%{http_code}' "$API/synonyms/boothost/NEWTAG/claim"; }
R1=$(claim); C1=$(tail -1 <<<"$R1"); B1=$(sed '$d' <<<"$R1")
echo "$B1" | python3 -m json.tool | head -30
[ "$C1" = 201 ] || fail "the open claim answered $C1"
GOLDEN=$(j 'd["host_golden"]["volume"]' <<<"$B1")
[ "$(j 'd["host_golden"]["minted"]' <<<"$B1")" = True ] || fail "no host golden minted"
[ "$(j 'd["claimed_from"]["release"]' <<<"$B1")" = "$RELEASE" ] || fail "not from the default release"
[ "$GOLDEN" != "$RELEASE" ] || fail "the machine was handed the release itself"
[ "$(j 'd["volume"]["name"]' <<<"$B1")" = boothost-NEWTAG ] || fail "the body chose the clone's name"

curl -s "$API/volumes" "${AUTH[@]}" > "$W/vols.json"
python3 - "$W/vols.json" "$GOLDEN" <<'PY' || fail "the host golden is not a sealed volume"
import json, sys
v = {x['id']: x for x in json.load(open(sys.argv[1]))['items']}
g = v[sys.argv[2]]
assert g.get('sealed'), g
print("host golden:", g['name'], "sealed")
PY
pinned=$(curl -s "$API/synonyms/boothost/NEWTAG" "${AUTH[@]}" | j 'd["volume"]["id"]')
[ "$pinned" = "$RELEASE" ] || fail "the new machine was not pinned to the default ($pinned)"
[ "$(code "$API/synonyms/evil/x" "${AUTH[@]}")" = 404 ] || fail "the open claim bound a name"

R2=$(claim); B2=$(sed '$d' <<<"$R2")
[ "$(tail -1 <<<"$R2")" = 201 ] || fail "second claim"
[ "$(j 'd["host_golden"]["volume"]' <<<"$B2")" = "$GOLDEN" ] || fail "the golden was not kept"
[ "$(j 'd["volume"]["id"]' <<<"$B2")" != "$(j 'd["volume"]["id"]' <<<"$B1")" ] || fail "not a fresh clone"
echo "second boot: same golden, fresh clone"

say "a restart keeps the token"
stop
start
[ "$(cat "$W/data/api_token")" = "$TOKEN" ] || fail "the token changed across a restart"
[ "$(code "$API/volumes" -H "Authorization: Bearer $TOKEN")" = 200 ] || fail "the kept token is refused"
[ "$(code "$API/volumes")" = 401 ] || fail "open after a restart"

say "PASS"
