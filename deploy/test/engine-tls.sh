#!/usr/bin/env bash
# engine-tls.sh <stormblock binary> <the golden's stormblock.toml> (#374,
# stormblock#483/#524/#525): the engine, as the build user, on the config the
# golden seals, with only its paths and ports moved to this job's scratch.
#
# A node's engine starts long before stormcert mints its pair, so this starts it
# with no pair and adds one: plain until then, both after it with no restart, and
# the boot port beside them answering the boot verbs and nothing else. Then
# forge's CA and CRL the same way (#497).
#
# Moved from stormcos deploy/test/engine-tls.sh with the golden's recipe (#755);
# run by ci-golden.sh on the tree deploy/build-image.sh writes. Issue numbers
# without a repo are stormcos's where they were (#374, #497, #571).
set -euo pipefail
bin="$1"; conf="$2"
fail() { echo "FAIL: engine-tls: $*" >&2; [ -f "${W:-}/engine.log" ] && tail -n 20 "$W/engine.log" >&2; exit 1; }
W=$(mktemp -d "${TMPDIR:-/tmp}/etls.XXXXXX")
pid=
trap '[ -n "$pid" ] && kill "$pid" 2>/dev/null; rm -rf "$W"' EXIT

free_port() { python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
API=$(free_port); BOOT=$(free_port); NVME=$(free_port)
C="$W/stormcert"; mkdir -p "$C" "$W/data"

python3 -I - "$conf" << 'PY' || fail "the golden's config does not name the node's pair, CA, optional mode and boot port"
import sys, tomllib
m = tomllib.load(open(sys.argv[1], "rb"))["management"]
want = {"tls_cert": "/etc/stormcert/stormblock.crt", "tls_key": "/etc/stormcert/stormblock.key",
        "tls_client_ca": "/etc/stormcert/ca.crt", "tls_mode": "optional", "boot_listen": "0.0.0.0:9089",
        "tls_admin_ca": "/etc/stormcert/forge-ca.crt", "tls_admin_crl": "/etc/stormcert/forge.crl",
        "tls_admin_subjects": ["stormcentral"]}
bad = {k: m.get(k) for k, v in want.items() if m.get(k) != v}
if bad: sys.exit(f"[management] differs: {bad}")
k = tomllib.load(open(sys.argv[1], "rb"))["kernel"]
want = {"client_cert": "/run/stormblock/system-data/identity/forge-node.crt.pem",
        "client_key": "/run/stormblock/system-data/identity/forge-node.key.pem",
        "forge_ca": "/run/stormblock/forge/apiserver-ca.crt"}
if k != want: sys.exit(f"[kernel] differs (#571): {k}")
PY
sed -e "s#/etc/stormcert/#$C/#g" -e "s#0\.0\.0\.0:9090#127.0.0.1:$API#" -e "s#0\.0\.0\.0:9089#127.0.0.1:$BOOT#" \
    -e "s#^data_dir = .*#data_dir = \"$W/data\"#" "$conf" > "$W/config.toml"
"$bin" --config "$W/config.toml" --data-dir "$W/data" --no-iscsi --nvmeof-addr "127.0.0.1:$NVME" > "$W/engine.log" 2>&1 &
pid=$!

plain="http://127.0.0.1:$API"; tls="https://127.0.0.1:$API"
field() { python3 -I -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {"d": d}))' "$1"; }
code() { curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$@" || true; }

for _ in $(seq 120); do
    kill -0 "$pid" 2>/dev/null || fail "the engine exited with no pair on disk (stormblock#483)"
    [ "$(code "$plain/api/v1/health")" = 200 ] && break
    sleep 0.5
done
h=$(curl -sf --max-time 10 "$plain/api/v1/health") || fail "no plain health with no pair"
[ "$(field 'd["tls"]["serving"]' <<< "$h")" = http ] || fail "no pair, but it does not say http: $h"
field 'd["tls"]["missing"]' <<< "$h" | grep -q tls_cert || fail "no pair, but tls_cert is not missing: $h"
echo "   no pair: the engine starts and serves plain"

# The boot port, whatever else is served (a bind is retried, so give it a moment).
for _ in $(seq 40); do [ "$(code "http://127.0.0.1:$BOOT/api/v1/health")" = 200 ] && break; sleep 0.5; done
[ "$(code "http://127.0.0.1:$BOOT/api/v1/health")" = 200 ] || fail "boot port: no health"
tok=$(cat "$W/data/api_token") || fail "no minted token in the data dir"
auth=(-H "Authorization: Bearer $tok")
[ "$(code "${auth[@]}" "http://127.0.0.1:$BOOT/api/v1/volumes?limit=1")" = 404 ] || fail "boot port serves the volumes API"

# stormcert's pair, late: as `stormcert-agent serving` lays it out in tier-0.
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/ca.key" -out "$W/ca.pem" -days 2 -subj /CN=node-ca 2> /dev/null
pair() { # name, extension line
    openssl req -newkey rsa:2048 -nodes -keyout "$W/$1.key" -out "$W/$1.csr" -subj "/CN=$1" 2> /dev/null
    printf '%s\n' "$2" > "$W/$1.ext"
    openssl x509 -req -in "$W/$1.csr" -CA "$W/ca.pem" -CAkey "$W/ca.key" -CAcreateserial -days 2 -extfile "$W/$1.ext" -out "$W/$1.crt" 2> /dev/null
}
pair stormblock 'subjectAltName=IP:127.0.0.1,DNS:localhost'
pair client 'extendedKeyUsage=clientAuth'
cp "$W/ca.pem" "$C/ca.crt"
install -m 0600 "$W/stormblock.key" "$C/stormblock.key"
cp "$W/stormblock.crt" "$C/stormblock.crt"

for _ in $(seq 120); do
    [ "$(code --cacert "$C/ca.crt" "$tls/api/v1/health")" = 200 ] && break
    kill -0 "$pid" 2>/dev/null || fail "the engine exited when the pair appeared"
    sleep 0.5
done
h=$(curl -sf --max-time 10 --cacert "$C/ca.crt" "$tls/api/v1/health") || fail "no https with the pair, no restart (stormblock#483)"
[ "$(field 'd["tls"]["serving"]' <<< "$h")" = https ] || fail "pair there, but it does not say https: $h"
[ "$(field 'd["tls"]["mode"]' <<< "$h")" = optional ] || fail "not optional: $h"
[ "$(field 'd["boot_listen"]["listening"]' <<< "$h")" = True ] || fail "boot port not listening: $h"

[ "$(code --cacert "$C/ca.crt" "${auth[@]}" "$tls/api/v1/volumes?limit=1")" = 200 ] || fail "https: the token is refused"
[ "$(code --cacert "$C/ca.crt" "$tls/api/v1/volumes?limit=1")" = 401 ] || fail "https: no credential is let in"
[ "$(code --cacert "$C/ca.crt" --cert "$W/client.crt" --key "$W/client.key" "$tls/api/v1/volumes?limit=1")" = 200 ] \
    || fail "https: the node CA's client certificate is not a credential"
# A caller that has not moved: the whole point of `optional` during the move.
[ "$(code "${auth[@]}" "$plain/api/v1/volumes?limit=1")" = 200 ] || fail "plain: a caller that has not moved is refused"
# Another CA's server view: an unverifiable engine is not reached by a checking client.
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/o.key" -out "$W/o.pem" -days 2 -subj /CN=other 2> /dev/null
[ "$(code --cacert "$W/o.pem" "$tls/api/v1/health")" = 000 ] || fail "a client trusting another CA reached it"
curl -sf --max-time 10 --cacert "$C/ca.crt" "${auth[@]}" "$tls/metrics" | grep -q '^stormblock_mgmt_plain_requests_total{' \
    || fail "no plain-request count on /metrics: the exit test of the move"
[ "$(code "http://127.0.0.1:$BOOT/api/v1/health")" = 200 ] || fail "boot port went away with TLS on"

# Forge's certificates (#497): nothing until both of forge's files are there, then
# CN stormcentral alone, until forge revokes it. As forge-trust lays them out in
# tier-0: PEM, and a CRL only forge's CA signed.
F="$W/forge"; mkdir -p "$F"; : > "$F/index"; echo 01 > "$F/crlnumber"
cat > "$F/ca.cnf" << CNF
[ca]
default_ca = d
[d]
database = $F/index
crlnumber = $F/crlnumber
certificate = $F/ca.pem
private_key = $F/ca.key
default_md = sha256
default_crl_days = 2
CNF
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$F/ca.key" -out "$F/ca.pem" -days 2 -subj /CN=forge-ca 2> /dev/null
fpair() { # file name, CN: a forge client certificate has no SAN (stormcert#117)
    openssl req -newkey rsa:2048 -nodes -keyout "$F/$1.key" -out "$F/$1.csr" -subj "/CN=$2" 2> /dev/null
    printf 'extendedKeyUsage=clientAuth\n' > "$F/$1.ext"
    openssl x509 -req -in "$F/$1.csr" -CA "$F/ca.pem" -CAkey "$F/ca.key" -CAcreateserial -days 2 -extfile "$F/$1.ext" -out "$F/$1.crt" 2> /dev/null
}
fpair sc stormcentral; fpair other storm-node-1
sc() { code --cacert "$C/ca.crt" --cert "$F/sc.crt" --key "$F/sc.key" "$tls/api/v1/volumes?limit=1"; }
other() { code --cacert "$C/ca.crt" --cert "$F/other.crt" --key "$F/other.key" "$tls/api/v1/volumes?limit=1"; }
waitfor() { # wanted code, what: the engine looks at its files every few seconds
    for _ in $(seq 40); do [ "$(sc)" = "$1" ] && return 0; sleep 0.5; done
    fail "$2: CN stormcentral got $(sc), want $1"
}
[ "$(sc)" != 200 ] || fail "CN stormcentral is let in with no forge CA on the node"
cp "$F/ca.pem" "$C/forge-ca.crt"; sleep 6
[ "$(sc)" != 200 ] || fail "CN stormcentral is let in with forge's CA and no CRL: a missing CRL must fail closed"
echo "   forge: no CA, then no CRL: CN stormcentral is refused"
openssl ca -config "$F/ca.cnf" -gencrl -out "$C/forge.crl" 2> /dev/null
waitfor 200 "forge's CA and CRL there, no restart"
[ "$(other)" != 200 ] || fail "any other forge certificate is let in (got $(other))"
# Admin, not merely let in: a destructive verb on a volume that isn't there is the
# engine's 404 for CN stormcentral, and the gate's refusal for anyone else.
del() { code --cacert "$C/ca.crt" "$@" -X DELETE "$tls/api/v1/volumes/00000000-0000-0000-0000-000000000000"; }
d=$(del --cert "$F/sc.crt" --key "$F/sc.key")
case "$d" in 401|403|000) fail "CN stormcentral may not make a destructive call (got $d): not admin" ;; esac
o=$(del --cert "$F/other.crt" --key "$F/other.key")
case "$o" in 401|403) ;; *) fail "another forge CN reached a destructive verb (got $o)" ;; esac
echo "   forge: DELETE of a missing volume: CN stormcentral $d, another forge CN $o"
[ "$(code --cacert "$C/ca.crt" --cert "$W/client.crt" --key "$W/client.key" "$tls/api/v1/volumes?limit=1")" = 200 ] \
    || fail "the node CA's client certificate stopped working with forge's CA there"
echo "   forge: CA + CRL: CN stormcentral admin, no restart; another CN nothing; the node CA's still works"
openssl ca -config "$F/ca.cnf" -revoke "$F/sc.crt" 2> /dev/null
openssl ca -config "$F/ca.cnf" -gencrl -out "$F/new.crl" 2> /dev/null
mv "$F/new.crl" "$C/forge.crl"
for _ in $(seq 40); do [ "$(sc)" != 200 ] && break; sleep 0.5; done
[ "$(sc)" != 200 ] || fail "forge revoked CN stormcentral and it is still let in"
echo "   forge: revoked by a new CRL: refused, no restart"
kill -0 "$pid" 2>/dev/null || fail "the engine exited during the forge steps"
grep -iE 'tls_admin|forge-ca|forge\.crl' "$W/engine.log" | cut -c1-200 | head -8 || true
echo "engine-tls: PASS (no pair = plain; pair = https + plain, no restart; client cert, token, count; boot port verbs only; forge CN stormcentral only with CA + CRL, until revoked)"
