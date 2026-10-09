#!/usr/bin/env bash
# ci-redirect-import-verify.sh — a real cloud image imported from its
# canonical, redirecting URL (#113). Needs the internet and the built binary.
#
#   sc-build 'cargo build --locked && bash ci-redirect-import-verify.sh'
#
# Debian's cloud.debian.org answers every request with a 302 to a mirror
# (Fedora, Rocky and CentOS do the same); the engine used to fail the
# import with "HTTP 302". It must now land on the mirror, import the
# qcow2, and read the root filesystem inside it.
set -uo pipefail
URL=${URL:-https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2}
ROOT=$(pwd)
BIN=${STORMBLOCK_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/debug/stormblock}
mkdir -p "$ROOT/tmp"; export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-redirect.XXXXXX")
MGMT=$((20000 + RANDOM % 20000))
TOKEN=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
SB_PID=""
cleanup() { [ -n "$SB_PID" ] && kill "$SB_PID" 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT
die() { echo "FAIL: $*"; tail -20 "$W/engine.log" 2>/dev/null; exit 1; }
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }
[ -x "$BIN" ] || die "no binary at $BIN"
curl -sI -m 20 "$URL" | head -1 | grep -q ' 30[12378]' || { echo "SKIP: $URL does not redirect from here"; exit 2; }
echo "== $URL answers: $(curl -sI -m 20 "$URL" | head -1 | tr -d '\r')"

mkdir -p "$W/data"
truncate -s 8G "$W/d1.img"
cat > "$W/stormblock.toml" <<CFG
[management]
api_token = "$TOKEN"
admin_token = "$TOKEN"
listen_addr = "127.0.0.1:$MGMT"
data_dir = "$W/data"
discovery_disabled = true
ublk_transport = false
CFG
RUST_LOG=stormblock=info "$BIN" --config "$W/stormblock.toml" --no-iscsi --no-nvmeof > "$W/engine.log" 2>&1 &
SB_PID=$!
api() { curl -sf -m 300 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$@"; }
B="http://127.0.0.1:$MGMT/api/v1"
for _ in $(seq 1 150); do api "$B/health" >/dev/null 2>&1 && break; sleep 0.2; done
api -X POST "$B/slabs" -d "{\"device_path\":\"$W/d1.img\",\"role\":\"data\"}" >/dev/null || die "slab"

echo "== import from the canonical URL"
ID=$(api -X POST "$B/volumes/import" -d "{\"name\":\"debian-13\",\"url\":\"$URL\"}" | j 'd["id"]') || die "import refused"
t0=$(date +%s)
while :; do
    ST=$(api "$B/volumes/import/$ID") || die "status"
    case "$(echo "$ST" | j 'd["state"]')" in
        done) break ;;
        failed) die "import failed ($(echo "$ST" | j 'd.get("phase")')): $(echo "$ST" | j 'd.get("error")')" ;;
    esac
    [ $(( $(date +%s) - t0 )) -gt 1500 ] && die "import did not finish in 25 min"
    sleep 5
done
echo "$ST" | j '"format %s, %d bytes downloaded, %d written, fs %s" % (d.get("format"), d["downloaded_bytes"], d["written_bytes"], (d.get("fs") or {}).get("kind"))'
echo "$ST" | j '"\n".join("  filesystem: partition %s %s %s %s" % (f.get("partition"), f.get("kind"), f.get("label"), f.get("os") or "") for f in d.get("filesystems", []))'
[ "$(echo "$ST" | j 'd.get("format")')" = qcow2 ] || die "not read as qcow2"
[ "$(echo "$ST" | j '(d.get("fs") or {}).get("kind")')" = gpt ] || die "no partition table inside"
# The root filesystem found and read end to end. (Its OS name is not checked:
# Debian's /etc/os-release is a symlink the ext4 survey does not follow yet.)
echo "$ST" | j 'any(f.get("kind") == "ext4" and (f.get("walked") or {}).get("entries", 0) > 1000 for f in d.get("filesystems", []))' \
    | grep -q True || die "no ext4 root walked inside"
echo "$ST" | j '"  walked: %s" % [f.get("walked") for f in d.get("filesystems", [])]'

echo "ALL PASS"
