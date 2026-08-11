#!/bin/bash
# Per-volume cost once a controller/session already exists.
#
# This is the clone-per-container shape: the transport is established once,
# and each container costs only whatever it takes to surface one more device.
set -u

MGMT="${MGMT:-192.168.8.181:9090}"
TARGET_IP="${TARGET_IP:-192.168.8.181}"
NQN="${NQN:-nqn.2024.io.stormblock:sb-node1}"
NODE="${NODE:-sb-node1}"
ITER="${ITER:-10}"

now() { date +%s%N; }
api() { curl -s -m 20 "$@"; }

stats() {
    sort -n | awk -v name="$1" '
        {v[NR]=$1}
        END{
            if(NR==0){printf "%-26s no samples\n", name; exit}
            p50=v[int(NR*0.5)+((NR%2)?1:0)]; if(p50=="")p50=v[NR];
            printf "%-26s n=%-3d min=%7.1f  p50=%7.1f  max=%7.1f  (ms)\n",
                   name, NR, v[1]/1e6, p50/1e6, v[NR]/1e6
        }'
}

count_ns() { ls /dev/nvme0n* 2>/dev/null | wc -l; }

echo "=== per-volume cost on an ALREADY-CONNECTED NVMe-oF controller ==="
sudo nvme disconnect -n "$NQN" >/dev/null 2>&1
t0=$(now)
sudo nvme connect -t tcp -a "$TARGET_IP" -s 4420 -n "$NQN" >/dev/null 2>&1
t1=$(now)
echo "one-time controller connect: $(awk -v n=$((t1-t0)) 'BEGIN{printf "%.1f", n/1e6}') ms"
sleep 1
echo "namespaces before: $(count_ns)"
echo

: > /tmp/ha_create.txt; : > /tmp/ha_attach.txt; : > /tmp/ha_visible.txt; : > /tmp/ha_detach.txt
ids=()
for i in $(seq 1 "$ITER"); do
    before=$(count_ns)

    t0=$(now)
    id=$(api -X POST "http://$MGMT/v1/volumes" -H 'Content-Type: application/json' \
         -d "{\"name\":\"ha-$i-$$\",\"size_bytes\":67108864}" | grep -o '"id":"[^"]*"' | cut -d'"' -f4)
    t1=$(now)
    [ -z "$id" ] && { echo "create failed at $i"; break; }
    ids+=("$id")

    api -X POST "http://$MGMT/v1/volumes/$id/attach" -H 'Content-Type: application/json' \
        -d "{\"node\":\"$NODE\",\"mode\":\"read_write\"}" >/dev/null
    t2=$(now)

    # Wait for the kernel to surface one more namespace — no reconnect, no scan.
    deadline=$((SECONDS+20)); ok=0
    while [ $SECONDS -lt $deadline ]; do
        [ "$(count_ns)" -gt "$before" ] && { ok=1; break; }
        sleep 0.005
    done
    t3=$(now)
    [ $ok -eq 0 ] && { echo "namespace never appeared for $id"; break; }

    echo $((t1-t0)) >> /tmp/ha_create.txt
    echo $((t2-t1)) >> /tmp/ha_attach.txt
    echo $((t3-t1)) >> /tmp/ha_visible.txt
done

stats "create (control plane)"  < /tmp/ha_create.txt
stats "attach call"             < /tmp/ha_attach.txt
stats "PER-VOLUME until usable" < /tmp/ha_visible.txt
echo
echo "namespaces after: $(count_ns)"

echo
echo "--- detach (namespace withdrawal) ---"
for id in "${ids[@]}"; do
    before=$(count_ns)
    t0=$(now)
    api -X POST "http://$MGMT/v1/volumes/$id/detach" -H 'Content-Type: application/json' \
        -d "{\"node\":\"$NODE\"}" >/dev/null
    deadline=$((SECONDS+20))
    while [ $SECONDS -lt $deadline ]; do
        [ "$(count_ns)" -lt "$before" ] && break
        sleep 0.005
    done
    t1=$(now)
    echo $((t1-t0)) >> /tmp/ha_detach.txt
    api -X DELETE "http://$MGMT/v1/volumes/$id" >/dev/null
done
stats "PER-VOLUME detach"       < /tmp/ha_detach.txt

sudo nvme disconnect -n "$NQN" >/dev/null 2>&1
