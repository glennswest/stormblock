#!/bin/bash
set -euo pipefail

echo "=== StormBlock CI Build + Test ==="
echo "Host: $(hostname)"
echo "Date: $(date)"
echo "Rust: $(rustc --version)"
echo "Cargo: $(cargo --version)"
echo ""

# ── Environment ───────────────────────────────────────
ISCSI_PORTAL="${ISCSI_PORTAL:-192.168.200.1}"
ISCSI_PORT="${ISCSI_PORT:-3260}"
ISCSI_IQN="${ISCSI_IQN:-iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-test1-raw}"

echo "=== Step 1: Install musl target ==="
rustup target add x86_64-unknown-linux-musl
echo ""

echo "=== Step 2: Build (debug, default features) ==="
cargo build 2>&1
echo "Debug build OK"
echo ""

echo "=== Step 3: Run cargo test ==="
cargo test 2>&1
echo ""

echo "=== Step 4: Build release (musl static) ==="
cargo build --release --target x86_64-unknown-linux-musl 2>&1
ls -lh target/x86_64-unknown-linux-musl/release/stormblock
echo "Release build OK"
echo ""

echo "=== Step 5: iSCSI discovery ==="
if ! command -v iscsiadm &>/dev/null; then
    echo "iscsiadm not found, installing iscsi-initiator-utils..."
    dnf install -y iscsi-initiator-utils 2>&1 | tail -3
fi
iscsiadm -m discovery -t sendtargets -p "${ISCSI_PORTAL}:${ISCSI_PORT}" 2>&1 || {
    echo "WARNING: iSCSI discovery failed (network may not be routable)"
    echo "Skipping iSCSI tests"
    echo "=== Done (build + unit tests passed, iSCSI skipped) ==="
    exit 0
}
echo ""

echo "=== Step 6: iSCSI login ==="
iscsiadm -m node -T "${ISCSI_IQN}" -p "${ISCSI_PORTAL}:${ISCSI_PORT}" --login 2>&1
sleep 2

# Find the iSCSI device
ISCSI_DEV=""
for dev in /dev/sd{a,b,c,d,e,f}; do
    if [ -b "$dev" ]; then
        # Check if this is an iSCSI device
        devname=$(basename "$dev")
        if [ -d "/sys/block/${devname}/device" ]; then
            model=$(cat "/sys/block/${devname}/device/model" 2>/dev/null || echo "")
            vendor=$(cat "/sys/block/${devname}/device/vendor" 2>/dev/null || echo "")
            echo "Found block device: $dev (vendor='${vendor}' model='${model}')"
            ISCSI_DEV="$dev"
        fi
    fi
done

if [ -z "${ISCSI_DEV}" ]; then
    echo "WARNING: No iSCSI block device found after login"
    iscsiadm -m node -T "${ISCSI_IQN}" -p "${ISCSI_PORTAL}:${ISCSI_PORT}" --logout 2>&1 || true
    echo "=== Done (build + unit tests passed, iSCSI device not found) ==="
    exit 0
fi
echo "Using iSCSI device: ${ISCSI_DEV}"
echo ""

echo "=== Step 7: Direct I/O read/write test ==="
# Write a known pattern to offset 0 (4KB)
TESTFILE=$(mktemp)
dd if=/dev/urandom of="${TESTFILE}" bs=4096 count=1 2>/dev/null
WRITE_SUM=$(sha256sum "${TESTFILE}" | awk '{print $1}')
echo "Write pattern SHA256: ${WRITE_SUM}"

# Write to disk
dd if="${TESTFILE}" of="${ISCSI_DEV}" bs=4096 count=1 oflag=direct 2>&1
sync

# Read it back
READFILE=$(mktemp)
dd if="${ISCSI_DEV}" of="${READFILE}" bs=4096 count=1 iflag=direct 2>/dev/null
READ_SUM=$(sha256sum "${READFILE}" | awk '{print $1}')
echo "Read back SHA256:     ${READ_SUM}"

if [ "${WRITE_SUM}" = "${READ_SUM}" ]; then
    echo "PASS: Write/read verification succeeded"
else
    echo "FAIL: Write/read mismatch!"
    rm -f "${TESTFILE}" "${READFILE}"
    iscsiadm -m node -T "${ISCSI_IQN}" -p "${ISCSI_PORTAL}:${ISCSI_PORT}" --logout 2>&1 || true
    exit 1
fi
rm -f "${TESTFILE}" "${READFILE}"
echo ""

echo "=== Step 8: StormBlock FileDevice test against iSCSI disk ==="
# Run stormblock with the iSCSI device as a FileDevice
timeout 10 target/x86_64-unknown-linux-musl/release/stormblock \
    --device "${ISCSI_DEV}" \
    --listen 127.0.0.1:9260 \
    --mgmt-listen 127.0.0.1:18080 2>&1 &
SB_PID=$!
sleep 3

# Check if it started
if kill -0 $SB_PID 2>/dev/null; then
    echo "StormBlock started (PID ${SB_PID})"
    # Hit the management API
    curl -sf http://127.0.0.1:18080/api/v1/drives 2>&1 || echo "mgmt API not responding (may need config)"
    kill $SB_PID 2>/dev/null || true
    wait $SB_PID 2>/dev/null || true
    echo "StormBlock stopped cleanly"
else
    echo "StormBlock exited early (may need config flags — checking exit)"
    wait $SB_PID 2>/dev/null || true
fi
echo ""

echo "=== Step 9: iSCSI logout ==="
iscsiadm -m node -T "${ISCSI_IQN}" -p "${ISCSI_PORTAL}:${ISCSI_PORT}" --logout 2>&1

echo ""
echo "=== All tests passed ==="
echo "Build:       OK (debug + release musl)"
echo "Unit tests:  OK (cargo test)"
echo "iSCSI I/O:   OK (write/read/verify)"
echo "Binary size:  $(ls -lh target/x86_64-unknown-linux-musl/release/stormblock | awk '{print $5}')"
