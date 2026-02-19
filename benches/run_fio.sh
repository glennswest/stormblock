#!/usr/bin/env bash
# Run fio benchmarks against StormBlock iSCSI and NVMe-oF targets.
# Requires: fio, iscsiadm (open-iscsi), nvme-cli. Linux only.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FIO_DIR="${SCRIPT_DIR}/fio"
RESULTS_DIR="${SCRIPT_DIR}/results/$(date +%Y%m%d-%H%M%S)"
TARGET_IP="${TARGET_IP:-127.0.0.1}"
ISCSI_PORT="${ISCSI_PORT:-3260}"
NVMEOF_PORT="${NVMEOF_PORT:-4420}"
ISCSI_TARGET="${ISCSI_TARGET:-iqn.2024.io.stormblock:default}"
NVMEOF_NQN="${NVMEOF_NQN:-nqn.2024.io.stormblock:default}"

mkdir -p "$RESULTS_DIR"

echo "=== StormBlock fio benchmarks ==="
echo "Target: ${TARGET_IP}"
echo "Results: ${RESULTS_DIR}"
echo ""

# --- iSCSI benchmarks ---
if command -v iscsiadm &>/dev/null; then
    echo "--- iSCSI benchmarks ---"

    # Discover and login
    iscsiadm -m discovery -t sendtargets -p "${TARGET_IP}:${ISCSI_PORT}" || true
    iscsiadm -m node -T "$ISCSI_TARGET" -p "${TARGET_IP}:${ISCSI_PORT}" --login || true
    sleep 2

    # Find the iSCSI device
    ISCSI_DEV=$(lsblk -dnpo NAME,TRAN | grep iscsi | head -1 | awk '{print $1}')
    if [ -n "$ISCSI_DEV" ]; then
        echo "iSCSI device: $ISCSI_DEV"

        for job in "$FIO_DIR"/iscsi_*.fio; do
            name=$(basename "$job" .fio)
            echo "  Running: $name"
            fio "$job" --filename="$ISCSI_DEV" \
                --output-format=json \
                --output="${RESULTS_DIR}/${name}.json" 2>&1 | tail -1
        done

        # Logout
        iscsiadm -m node -T "$ISCSI_TARGET" -p "${TARGET_IP}:${ISCSI_PORT}" --logout || true
    else
        echo "  No iSCSI device found, skipping"
    fi
else
    echo "iscsiadm not found, skipping iSCSI benchmarks"
fi

echo ""

# --- NVMe-oF benchmarks ---
if command -v nvme &>/dev/null; then
    echo "--- NVMe-oF benchmarks ---"

    # Connect
    nvme connect -t tcp -a "$TARGET_IP" -s "$NVMEOF_PORT" -n "$NVMEOF_NQN" || true
    sleep 2

    # Find the NVMe-oF device
    NVMEOF_DEV=$(nvme list 2>/dev/null | grep "$NVMEOF_NQN" | awk '{print $1}' | head -1)
    if [ -z "$NVMEOF_DEV" ]; then
        # Fallback: use the last NVMe device
        NVMEOF_DEV=$(ls /dev/nvme*n1 2>/dev/null | tail -1)
    fi

    if [ -n "$NVMEOF_DEV" ]; then
        echo "NVMe-oF device: $NVMEOF_DEV"

        for job in "$FIO_DIR"/nvmeof_*.fio; do
            name=$(basename "$job" .fio)
            echo "  Running: $name"
            fio "$job" --filename="$NVMEOF_DEV" \
                --output-format=json \
                --output="${RESULTS_DIR}/${name}.json" 2>&1 | tail -1
        done

        # Disconnect
        nvme disconnect -n "$NVMEOF_NQN" || true
    else
        echo "  No NVMe-oF device found, skipping"
    fi
else
    echo "nvme-cli not found, skipping NVMe-oF benchmarks"
fi

echo ""
echo "=== Results saved to ${RESULTS_DIR} ==="
ls -la "$RESULTS_DIR"
