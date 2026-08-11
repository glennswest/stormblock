#!/bin/bash
# Connection/protocol overhead: iSCSI vs NVMe-oF/TCP.
#
# Measures the phases of "get me a usable block device" and "give it back",
# which is what a clone-per-container workload pays on every start and stop.
set -u

TARGET_IP="${TARGET_IP:-192.168.8.181}"
IQN="${IQN:-iqn.2024.io.stormblock:sb-node1}"
NQN="${NQN:-nqn.2024.io.stormblock:sb-node1}"
ITER="${ITER:-10}"

now() { date +%s%N; }
ms() { awk -v n="$1" 'BEGIN{printf "%.1f", n/1000000}'; }

# Wait until a block device is not just present but openable.
wait_dev() {
    local pat="$1" deadline=$((SECONDS + 30)) d
    while [ $SECONDS -lt $deadline ]; do
        for d in $pat; do
            if [ -b "$d" ] && sudo dd if="$d" of=/dev/null bs=4096 count=1 iflag=direct >/dev/null 2>&1; then
                echo "$d"; return 0
            fi
        done
        sleep 0.01
    done
    return 1
}

stats() { # name, then values on stdin
    sort -n | awk -v name="$1" '
        {v[NR]=$1; s+=$1}
        END{
            if(NR==0){printf "%-22s no samples\n", name; exit}
            p50=v[int(NR*0.5)+((NR%2)?1:0)]; if(p50=="")p50=v[NR];
            p95=v[int(NR*0.95)]; if(p95=="")p95=v[NR];
            printf "%-22s n=%-3d min=%8.1f  p50=%8.1f  p95=%8.1f  max=%8.1f  (ms)\n",
                   name, NR, v[1]/1e6, p50/1e6, p95/1e6, v[NR]/1e6
        }'
}

echo "=== target $TARGET_IP  iterations $ITER ==="
echo

# ---------------------------------------------------------------- NVMe-oF
echo "--- NVMe-oF/TCP ---"
sudo nvme disconnect -n "$NQN" >/dev/null 2>&1
: > /tmp/nvme_conn.txt; : > /tmp/nvme_dev.txt; : > /tmp/nvme_disc.txt; : > /tmp/nvme_total.txt
for i in $(seq 1 "$ITER"); do
    t0=$(now)
    sudo nvme connect -t tcp -a "$TARGET_IP" -s 4420 -n "$NQN" >/dev/null 2>&1 || { echo "connect failed"; break; }
    t1=$(now)
    dev=$(wait_dev "/dev/nvme*n1") || { echo "no device"; break; }
    t2=$(now)
    sudo nvme disconnect -n "$NQN" >/dev/null 2>&1
    t3=$(now)
    echo $((t1-t0)) >> /tmp/nvme_conn.txt
    echo $((t2-t1)) >> /tmp/nvme_dev.txt
    echo $((t3-t2)) >> /tmp/nvme_disc.txt
    echo $((t2-t0)) >> /tmp/nvme_total.txt
    sleep 0.3
done
stats "nvme connect"     < /tmp/nvme_conn.txt
stats "nvme dev ready"   < /tmp/nvme_dev.txt
stats "nvme ATTACH TOTAL" < /tmp/nvme_total.txt
stats "nvme disconnect"  < /tmp/nvme_disc.txt
echo

# ---------------------------------------------------------------- iSCSI
echo "--- iSCSI ---"
sudo iscsiadm -m node -T "$IQN" -p "$TARGET_IP":3260 --logout >/dev/null 2>&1
sudo iscsiadm -m node -o delete -T "$IQN" -p "$TARGET_IP":3260 >/dev/null 2>&1
: > /tmp/i_disc.txt; : > /tmp/i_login.txt; : > /tmp/i_dev.txt; : > /tmp/i_out.txt; : > /tmp/i_total.txt
: > /tmp/i_warm.txt
for i in $(seq 1 "$ITER"); do
    # cold: no cached node record, as a first-ever attach on a host
    sudo iscsiadm -m node -o delete -T "$IQN" -p "$TARGET_IP":3260 >/dev/null 2>&1
    t0=$(now)
    sudo iscsiadm -m discovery -t sendtargets -p "$TARGET_IP":3260 >/dev/null 2>&1
    t1=$(now)
    sudo iscsiadm -m node -T "$IQN" -p "$TARGET_IP":3260 --login >/dev/null 2>&1 || { echo "login failed"; break; }
    t2=$(now)
    dev=$(wait_dev "/dev/disk/by-path/*$IQN*lun-0") || { echo "no device"; break; }
    t3=$(now)
    sudo iscsiadm -m node -T "$IQN" -p "$TARGET_IP":3260 --logout >/dev/null 2>&1
    t4=$(now)
    echo $((t1-t0)) >> /tmp/i_disc.txt
    echo $((t2-t1)) >> /tmp/i_login.txt
    echo $((t3-t2)) >> /tmp/i_dev.txt
    echo $((t4-t3)) >> /tmp/i_out.txt
    echo $((t3-t0)) >> /tmp/i_total.txt
    sleep 0.3
done
stats "iscsi discovery"    < /tmp/i_disc.txt
stats "iscsi login"        < /tmp/i_login.txt
stats "iscsi dev ready"    < /tmp/i_dev.txt
stats "iscsi ATTACH TOTAL" < /tmp/i_total.txt
stats "iscsi logout"       < /tmp/i_out.txt
echo

# warm: node record already cached, so discovery is skipped
echo "--- iSCSI warm (node record cached, no discovery) ---"
sudo iscsiadm -m discovery -t sendtargets -p "$TARGET_IP":3260 >/dev/null 2>&1
for i in $(seq 1 "$ITER"); do
    t0=$(now)
    sudo iscsiadm -m node -T "$IQN" -p "$TARGET_IP":3260 --login >/dev/null 2>&1 || break
    dev=$(wait_dev "/dev/disk/by-path/*$IQN*lun-0") || break
    t1=$(now)
    sudo iscsiadm -m node -T "$IQN" -p "$TARGET_IP":3260 --logout >/dev/null 2>&1
    echo $((t1-t0)) >> /tmp/i_warm.txt
    sleep 0.3
done
stats "iscsi warm ATTACH"  < /tmp/i_warm.txt
