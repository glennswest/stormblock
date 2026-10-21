#!/usr/bin/env bash
# ci-cold-claim-timing.sh — the engine calls a kubelet's PVC claim makes,
# timed cold (the first claim after the engine starts) and warm, on several
# engines (#472).
#
# Dell 12.14 → 12.37: the cold claim's `lookup` 119 → 161 ms and `blank`
# 146 → 206 ms; warm fine. rustkube-node's claim (pod_manager.rs) asks:
#
#   lookup  GET /api/v1/volumes, GET /api/v1/slabs, GET /api/v1/volumes
#   blank   GET /api/v1/fstemplates/{name}, POST /api/v1/fstemplates/{id}/clone
#
# Each engine is given the same node: a RAID-1 slab on two files, the size-
# class blanks, $V volumes beside them (a node carries ~150). Then, $R times:
# stop it, start it, wait for health, run one claim (cold), then $N more
# (warm). The medians per call, per engine, are printed side by side.
#
#   ENGINES="old=/path/to/bin new=/path/to/bin" ./ci-cold-claim-timing.sh
#   sc-build 'sh ci-cold-claim-timing.sh build 7911dd9 5062192 HEAD'
#
# With `build <rev>…`, each rev is built (`cargo build --locked --release`)
# in a worktree of this checkout and its binary used.
set -euo pipefail

W=${WORK:-$PWD/tmp/cold-claim}
MGMT=${MGMT:-127.0.0.1:9197}
V=${V:-150}
R=${R:-5}
N=${N:-5}
API="http://$MGMT/api/v1"

fail() { printf '\nFAIL: %s\n' "$*" >&2; [ -f "$W/engine.log" ] && tail -20 "$W/engine.log" >&2; exit 1; }
ENGINE=
trap '[ -n "$ENGINE" ] && kill -9 "$ENGINE" 2>/dev/null; wait 2>/dev/null || true' EXIT
mkdir -p "$W"

if [ "${1:-}" = build ]; then
    shift
    ENGINES=
    for rev in "$@"; do
        git cat-file -e "$rev^{commit}" 2>/dev/null || { url=${REPO_URL:-https://github.com/glennswest/stormblock}
            if [ "$(git rev-parse --is-shallow-repository)" = true ]; then git fetch -q --unshallow "$url" main; else git fetch -q "$url" main; fi; }
        sha=$(git rev-parse --short "$rev")
        tree="$W/src-$sha"
        rm -rf "$tree"; git worktree prune
        git worktree add -q --detach "$tree" "$sha"
        echo "building $rev ($sha)…"
        (cd "$tree" && cargo build --locked --release -q --bin stormblock) || fail "build of $rev"
        tgt=$(cd "$tree" && cargo metadata --format-version 1 --no-deps -q | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
        cp "$tgt/release/stormblock" "$W/stormblock-$sha"
        ENGINES="$ENGINES $rev=$W/stormblock-$sha"
    done
fi
ENGINES=${ENGINES:-new=${CARGO_TARGET_DIR:-target}/release/stormblock}

start() {
    RUST_LOG=stormblock=warn "$BIN" --config "$W/node/stormblock.toml" --device "$W/node/d1.img" --device "$W/node/d2.img" \
        --raid raid1 --data-dir "$W/node/data" --no-iscsi --nvmeof-addr 127.0.0.1:4437 \
        --nvmeof-nqn nqn.2026-10.lo.test:cold >>"$W/engine.log" 2>&1 &
    ENGINE=$!
    for _ in $(seq 1 1200); do
        curl -s -o /dev/null "$API/health" && return 0
        kill -0 "$ENGINE" 2>/dev/null || fail "engine exited"
        sleep 0.05
    done
    fail "engine never answered"
}
stop() { kill -TERM "$ENGINE"; wait "$ENGINE" 2>/dev/null || true; ENGINE=; }
H=(-H 'Authorization: Bearer t' -H 'Content-Type: application/json')
# ms for one request; the body to $W/last.
ms() { curl -s -o "$W/last" -w '%{time_total} %{http_code}' "${H[@]}" "$@" | awk '{ if ($2 !~ /^2/) { print "HTTP " $2 > "/dev/stderr"; exit 1 } printf "%.1f", $1 * 1000 }'; }
j() { python3 -c "import json,sys; d=json.load(open(sys.argv[2])); print(eval(sys.argv[1]))" "$1" "$W/last"; }

# One claim as the kubelet makes it: "lookup blank vols slabs tmpl clone".
claim() {
    local a b c d e
    a=$(ms "$API/volumes") || fail "GET volumes"
    b=$(ms "$API/slabs") || fail "GET slabs"
    c=$(ms "$API/volumes") || fail "GET volumes"
    d=$(ms "$API/fstemplates/pvc-ext4j-64m") || fail "GET template"
    tid=$(j 'd.get("id") or d["template"]["id"]')
    e=$(ms -X POST "$API/fstemplates/$tid/clone" -d "{\"name\":\"claim-$1\",\"verify\":true}") || fail "clone"
    echo "$(awk -v a="$a" -v b="$b" -v c="$c" 'BEGIN{print a+b+c}') $(awk -v d="$d" -v e="$e" 'BEGIN{print d+e}') $a $b $c $d $e"
}

COLS="lookup blank vol1 slabs vol2 tmpl clone"
declare -A COLD WARM
for spec in $ENGINES; do
    name=${spec%%=*}; BIN=${spec#*=}
    [ -x "$BIN" ] || fail "no binary for $name at $BIN"
    rm -rf "$W/node"; mkdir -p "$W/node/data"; : > "$W/engine.log"
    truncate -s 32G "$W/node/d1.img" "$W/node/d2.img"
    cat > "$W/node/stormblock.toml" <<EOF
[management]
listen_addr = "$MGMT"
data_dir = "$W/node/data"
api_token = "t"
admin_token = "t"
node_name = "ci-cold"
discovery_disabled = true
EOF
    start
    for sz in 64m 256m 1g; do
        ms -m 300 -X POST "$API/fstemplates" -d "{\"name\":\"pvc-ext4j-$sz\",\"size\":\"${sz^^}\"}" >/dev/null || fail "template $sz"
    done
    ms "$API/fstemplates/pvc-ext4j-64m" >/dev/null; tid=$(j 'd.get("id") or d["template"]["id"]')
    for i in $(seq 1 "$V"); do
        ms -X POST "$API/fstemplates/$tid/clone" -d "{\"name\":\"node-vol-$i\"}" >/dev/null || fail "clone $i"
    done
    ms "$API/volumes" >/dev/null
    echo "== $name ($("$BIN" --version)): $(j 'len(d["items"])') volumes"
    cold=() warm=()
    for r in $(seq 1 "$R"); do
        stop; start
        cold+=("$(claim "c$r")")
        for n in $(seq 1 "$N"); do warm+=("$(claim "w$r-$n")"); done
    done
    stop
    printf '%s\n' "${cold[@]}" > "$W/$name.cold"
    printf '%s\n' "${warm[@]}" > "$W/$name.warm"
done

echo
echo "median ms per call (cold = the first claim after the engine starts; $R starts, $N warm claims each)"
python3 - "$W" "$COLS" $ENGINES <<'PY'
import statistics, sys
w, cols, specs = sys.argv[1], sys.argv[2].split(), [s.split("=")[0] for s in sys.argv[3:]]
for kind in ("cold", "warm"):
    print(f"\n{kind:12} " + "".join(f"{c:>9}" for c in cols))
    for n in specs:
        rows = [list(map(float, l.split())) for l in open(f"{w}/{n}.{kind}") if l.strip()]
        med = [statistics.median(r[i] for r in rows) for i in range(len(cols))]
        print(f"{n[:12]:12} " + "".join(f"{m:9.1f}" for m in med))
PY
