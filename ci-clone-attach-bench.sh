#!/bin/bash
# ci-clone-attach-bench.sh — clone-and-attach latency for #4.
#
# #4 asks for p50/p99 on the disk half of a VM/container fork, with a ms-class
# target, and specifically for clone latency vs volume size (it should be O(1),
# since a clone is an extent-map operation and copies no data).
#
# Measures four operations against a running engine:
#   clone   POST /v1/volumes with source        — the CoW clone
#   attach  POST /v1/volumes/{id}/attach        — hot-add + NSID assignment
#   reset   POST /v1/volumes/{id}/reset         — squash divergence
#   delete  DELETE /v1/volumes/{id}
#
# Clone is measured at several golden-image sizes to show whether it is
# actually O(1) or secretly O(extents).

set -uo pipefail

MGMT="${MGMT:-127.0.0.1:9090}"
NODE="${NODE:-bench-node}"
ITERS="${ITERS:-100}"

GREEN='' BOLD='' RESET='' CYAN=''
if [ -t 1 ]; then GREEN='\033[0;32m'; BOLD='\033[1m'; RESET='\033[0m'; CYAN='\033[0;36m'; fi
hdr() { echo; echo -e "${BOLD}${CYAN}── $1 ──${RESET}"; echo; }

api() { curl -s -m 30 "$@"; }
# Milliseconds for one request.
timed() { curl -s -o /dev/null -m 30 -w '%{time_total}' "$@" | awk '{printf "%.3f", $1*1000}'; }

# p50/p99/mean from a newline-separated list of milliseconds.
stats() {
    sort -n | awk '
      {v[NR]=$1; s+=$1}
      END {
        if (NR==0) { print "no samples"; exit }
        p50=v[int(NR*0.50)+((NR*0.50)==int(NR*0.50)?0:1)]
        p99=v[int(NR*0.99)+((NR*0.99)==int(NR*0.99)?0:1)]
        printf "n=%-4d mean=%7.2f ms   p50=%7.2f ms   p99=%7.2f ms   max=%7.2f ms\n",
               NR, s/NR, p50, p99, v[NR]
      }'
}

echo -e "${BOLD}StormBlock — clone-and-attach latency (#4)${RESET}"
echo "engine: $MGMT   iterations: $ITERS   $(date)"

api "http://$MGMT/api/v1/drives" >/dev/null 2>&1 || { echo "engine not reachable at $MGMT"; exit 2; }

# ── Does per-op latency depend on how many volumes already exist? ───────────

hdr "Clone latency as the volume count grows"
echo "  A clone is an extent-map operation, so it should not care how many"
echo "  other volumes exist. If it does, something is O(total volumes)."
printf "  %-14s %s\n" "volumes" "clone latency"

for MB in 8 32 128; do
    G=$(api -X POST "http://$MGMT/v1/volumes" -H 'Content-Type: application/json' \
        -d "{\"name\":\"golden-$MB\",\"size_bytes\":$((512*1024*1024)),\"replica_tier\":{\"slaves\":0}}")
    GID=$(echo "$G" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p' | head -1)
    [ -z "$GID" ] && { echo "  golden create failed: $G"; continue; }

    SAMPLES=""
    for i in $(seq 1 20); do
        T=$(timed -X POST "http://$MGMT/v1/volumes" -H 'Content-Type: application/json' \
            -d "{\"name\":\"c-$MB-$i\",\"size_bytes\":$((512*1024*1024)),\"replica_tier\":{\"slaves\":0},\"source\":{\"kind\":\"volume\",\"id\":\"$GID\"}}")
        SAMPLES="$SAMPLES$T\n"
    done
    NVOL=$(api "http://$MGMT/v1/volumes" | grep -o '"id"' | wc -l | tr -d ' ')
    printf "  %-14s " "~$NVOL total"
    printf "$SAMPLES" | grep -v '^$' | stats
done

# ── The hot loop: clone → attach → reset → detach → delete ──────────────────

hdr "Per-operation latency over $ITERS iterations"

GOLD=$(api -X POST "http://$MGMT/v1/volumes" -H 'Content-Type: application/json' \
    -d "{\"name\":\"bench-golden\",\"size_bytes\":$((256*1024*1024)),\"replica_tier\":{\"slaves\":0}}")
GID=$(echo "$GOLD" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p' | head -1)
[ -z "$GID" ] && { echo "golden create failed"; exit 1; }

CLONE_T=""; ATTACH_T=""; RESET_T=""; DELETE_T=""; E2E_T=""

for i in $(seq 1 "$ITERS"); do
    # Body and timing from one request — the create response carries the id,
    # so no lookup round trip lands inside the measured window.
    R=$(curl -s -m 30 -w '\n%{time_total}' -X POST "http://$MGMT/v1/volumes" \
        -H 'Content-Type: application/json' \
        -d "{\"name\":\"bench-$i\",\"size_bytes\":$((256*1024*1024)),\"replica_tier\":{\"slaves\":0},\"source\":{\"kind\":\"volume\",\"id\":\"$GID\"}}")
    CT=$(printf '%s' "$R" | tail -1 | awk '{printf "%.3f", $1*1000}')
    VID=$(printf '%s' "$R" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p' | head -1)
    CLONE_T="$CLONE_T$CT\n"
    [ -z "$VID" ] && continue

    T=$(timed -X POST "http://$MGMT/v1/volumes/$VID/attach" -H 'Content-Type: application/json' \
        -d "{\"node\":\"$NODE\",\"mode\":\"read_write\"}")
    ATTACH_T="$ATTACH_T$T\n"

    E2E_T="$E2E_T$(awk -v a="$CT" -v b="$T" 'BEGIN{printf "%.3f", a+b}')\n"

    api -X POST "http://$MGMT/v1/volumes/$VID/detach" -H 'Content-Type: application/json' \
        -d "{\"node\":\"$NODE\"}" >/dev/null

    T=$(timed -X POST "http://$MGMT/v1/volumes/$VID/reset")
    RESET_T="$RESET_T$T\n"

    T=$(timed -X DELETE "http://$MGMT/v1/volumes/$VID")
    DELETE_T="$DELETE_T$T\n"
done

printf "  %-22s " "clone";            printf "$CLONE_T"  | grep -v '^$' | stats
printf "  %-22s " "attach (hot-add)"; printf "$ATTACH_T" | grep -v '^$' | stats
printf "  %-22s " "reset";            printf "$RESET_T"  | grep -v '^$' | stats
printf "  %-22s " "delete";           printf "$DELETE_T" | grep -v '^$' | stats
echo
printf "  %-22s " "clone+attach (e2e)"; printf "$E2E_T" | grep -v '^$' | stats

hdr "Verdict"
P50=$(printf "$E2E_T" | grep -v '^$' | sort -n | awk '{v[NR]=$1} END {print v[int(NR*0.5)+1]}')
echo "  #4 targets ms-class p50 for clone-and-attach (~1-2 ms budget)."
awk -v p="$P50" 'BEGIN {
  if (p < 2)      printf "  p50 = %.2f ms — within the 1-2 ms budget\n", p
  else if (p < 10) printf "  p50 = %.2f ms — ms-class, above the 1-2 ms budget\n", p
  else             printf "  p50 = %.2f ms — NOT ms-class\n", p
}'
echo
echo "  Note: these are control-plane latencies over HTTP against the engine."
echo "  A real fork also pays the CSI round trip and the node-side device"
echo "  appearing, which are not measured here."
