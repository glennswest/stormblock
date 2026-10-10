#!/usr/bin/env bash
# ci-emulated-format-verify.sh — an ext4 template on emulated drives through
# the served engine's API, memory store against directory backing (#300).
#
# stormcos#92 saw `POST /api/v1/fstemplates` of 1T on two directory-backed
# emulated 256T drives not finish in 180 s (5.5 s on the memory store), with
# the watchdog saying the API runtime had stopped running its heartbeat. This
# runs the built daemon as a node runs it: per store, two emulated 256T drives
# made slabs through the API, then a template of each size (default 1T,16T)
# formatted in core and sealed. Meanwhile `/api/v1/health` is asked every
# second, and its slowest answer is kept.
#
# Pass: every template is ready; directory backing takes at most
# max(3 × memory, memory + 30 s); health never takes over 2 s; the engine logs
# no stalled heartbeat.
#
#   sc-build 'cargo build --locked --release && ./ci-emulated-format-verify.sh'
set -uo pipefail

BIN=${STORMBLOCK_BIN:-${CARGO_TARGET_DIR:-target}/release/stormblock}
W=${WORK:-$PWD/tmp/emulated-format}
MGMT=${MGMT:-127.0.0.1:9196}
API="http://$MGMT/api/v1"
SIZES=${SIZES:-1T,16T}
FAILS=0
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
say() { printf '\n== %s\n' "$*"; }
ENGINE= SAMPLER=
trap '[ -n "$SAMPLER" ] && kill "$SAMPLER" 2>/dev/null; [ -n "$ENGINE" ] && kill -9 "$ENGINE" 2>/dev/null; wait 2>/dev/null || true' EXIT
[ -x "$BIN" ] || { echo "FAIL: no binary at $BIN"; exit 1; }
H=(-H 'Authorization: Bearer t' -H 'Content-Type: application/json')
now() { date +%s.%N; }
secs() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.1f", b-a}'; }

declare -A TOOK
run_store() {
    local store=$1
    rm -rf "$W/$store"; mkdir -p "$W/$store/data"
    cat > "$W/$store/stormblock.toml" <<EOT
[management]
listen_addr = "$MGMT"
data_dir = "$W/$store/data"
api_token = "t"
admin_token = "t"
node_name = "ci-emulated"
discovery_disabled = true
EOT
    RUST_LOG=stormblock=info "$BIN" --config "$W/$store/stormblock.toml" --data-dir "$W/$store/data" --no-iscsi \
        > "$W/$store/engine.log" 2>&1 &
    ENGINE=$!
    for _ in $(seq 1 300); do curl -s -o /dev/null "$API/health" && break; sleep 0.1; done
    for d in 0 1; do
        local uri="emulated://$store$d?size=256T"
        [ "$store" = dir ] && uri="$uri&backing=$W/$store/drive$d"
        out=$(curl -s -m 300 -w ' HTTP%{http_code}' -X POST "${H[@]}" "$API/slabs" \
            -d "{\"device_path\":\"$uri\",\"role\":\"data\"}")
        case "$out" in *HTTP2??) ;; *) fail "$store slab $d: $out"; tail -5 "$W/$store/engine.log" ;; esac
    done
    # Health asked every second while the templates are made; slowest kept.
    ( worst=0
      while :; do
          t=$(curl -s -o /dev/null -m 30 -w '%{time_total}' "$API/health" || echo 30)
          worst=$(awk -v a="$worst" -v b="$t" 'BEGIN{print (b>a)?b:a}')
          echo "$worst" > "$W/$store/health-worst"
          sleep 1
      done ) &
    SAMPLER=$!
    for size in ${SIZES//,/ }; do
        local t0 t1 code
        t0=$(now)
        code=$(curl -s -o "$W/$store/tmpl-$size.json" -m 1200 -w '%{http_code}' -X POST "${H[@]}" \
            "$API/fstemplates" -d "{\"name\":\"t-$size\",\"size\":\"$size\"}")
        t1=$(now)
        TOOK[$store-$size]=$(secs "$t0" "$t1")
        echo "  $store $size: HTTP $code in ${TOOK[$store-$size]} s"
        case "$code" in 2??) ;; *) fail "$store $size: HTTP $code $(head -c 300 "$W/$store/tmpl-$size.json")" ;; esac
    done
    kill "$SAMPLER" 2>/dev/null; wait "$SAMPLER" 2>/dev/null; SAMPLER=
    echo "  $store: slowest health answer $(cat "$W/$store/health-worst") s"
    awk -v w="$(cat "$W/$store/health-worst")" 'BEGIN{exit !(w>2)}' && fail "$store: health took $(cat "$W/$store/health-worst") s"
    if grep -qE "has not run its heartbeat|request\(s\) stalled" "$W/$store/engine.log"; then
        fail "$store: the engine logged a stall"
        grep -E "has not run its heartbeat|stalled" "$W/$store/engine.log" | head -3
    fi
    [ "$store" = dir ] && echo "  dir: $(du -sh "$W/$store" | cut -f1) on disk"
    kill -TERM "$ENGINE"; wait "$ENGINE" 2>/dev/null; ENGINE=
}

say "memory store"; run_store memory
say "directory backing"; run_store dir
say "result"
for size in ${SIZES//,/ }; do
    m=${TOOK[memory-$size]:-}; d=${TOOK[dir-$size]:-}
    echo "  $size: memory $m s, directory $d s"
    [ -n "$m" ] && [ -n "$d" ] || continue
    awk -v m="$m" -v d="$d" 'BEGIN{lim=(3*m > m+30) ? 3*m : m+30; exit !(d>lim)}' \
        && fail "$size: directory $d s against memory $m s"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
