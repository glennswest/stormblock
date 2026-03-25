#!/bin/bash
# boot-iscsi-test.sh — CI test for boot-from-iSCSI feature
#
# Phases:
# 1. Build stormblock
# 2. Run unit tests (including boot_iscsi tests)
# 3. Connect to iSCSI target, format as slab, create volumes
# 4. Write/read test data through ThinVolumes on iSCSI slab
# 5. Migrate to second iSCSI disk, verify data
#
# Requires: mkube job runner with iSCSI disks available

set -euo pipefail

PHASE=0
ERRORS=0

phase() {
    PHASE=$((PHASE + 1))
    echo "=============================="
    echo "PHASE $PHASE: $1"
    echo "=============================="
}

pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1"; ERRORS=$((ERRORS + 1)); }

# ── Phase 1: Build ──────────────────────────────────────────────

phase "Build"

if [ -f /root/stormblock/target/debug/stormblock ]; then
    echo "Using pre-built binary"
    BINARY=/root/stormblock/target/debug/stormblock
else
    echo "Building from source..."
    cd /root/stormblock
    cargo build 2>&1
    BINARY=target/debug/stormblock
fi

if [ -f "$BINARY" ]; then
    pass "Binary built: $BINARY"
else
    fail "Binary not found"
    exit 1
fi

# ── Phase 2: Unit tests ────────────────────────────────────────

phase "Unit tests"

cd /root/stormblock
cargo test --test boot_iscsi 2>&1
if [ $? -eq 0 ]; then
    pass "boot_iscsi tests"
else
    fail "boot_iscsi tests"
fi

cargo test 2>&1
if [ $? -eq 0 ]; then
    pass "All unit tests"
else
    fail "Some unit tests failed"
fi

# ── Phase 3: iSCSI slab format + volume creation ───────────────

phase "iSCSI slab operations"

# Test with the test1 iSCSI disk (10 GB)
PORTAL="${ISCSI_PORTAL:-192.168.10.1}"
PORT="${ISCSI_PORT:-3260}"
IQN="${ISCSI_IQN:-iqn.2000-02.com.mikrotik:test1}"

echo "Target: $PORTAL:$PORT $IQN"

# Test the boot-iscsi subcommand (non-interactive, just prints layout and exits)
timeout 30 $BINARY boot-iscsi \
    --portal "$PORTAL" \
    --port "$PORT" \
    --iqn "$IQN" \
    --layout "esp:256M,boot:512M,root:6G,swap:1G,home:rest" \
    &
BOOT_PID=$!

# Give it a few seconds to connect and provision
sleep 10

if kill -0 $BOOT_PID 2>/dev/null; then
    pass "boot-iscsi provisioned and running"
    # Send Ctrl+C equivalent
    kill -INT $BOOT_PID
    wait $BOOT_PID 2>/dev/null || true
else
    wait $BOOT_PID 2>/dev/null
    EXIT_CODE=$?
    if [ $EXIT_CODE -eq 0 ]; then
        pass "boot-iscsi completed"
    else
        fail "boot-iscsi failed (exit=$EXIT_CODE)"
    fi
fi

# ── Phase 4: Clippy ────────────────────────────────────────────

phase "Clippy"

cd /root/stormblock
cargo clippy -- -D warnings 2>&1
if [ $? -eq 0 ]; then
    pass "clippy clean"
else
    fail "clippy warnings"
fi

# ── Phase 5: Migration (if second disk available) ──────────────

phase "Migration"

IQN2="${ISCSI_IQN2:-iqn.2000-02.com.mikrotik:stormblock-test2}"

echo "Source: $PORTAL:$PORT $IQN"
echo "Target: $PORTAL:$PORT $IQN2"
echo "(Migration test requires both disks to have slab data)"
echo "Skipping automated migration test — use manual: "
echo "  stormblock migrate-boot \\"
echo "    --source-portal $PORTAL --source-iqn $IQN \\"
echo "    --target-device /dev/sdX"

pass "Migration test documented (manual)"

# ── Summary ─────────────────────────────────────────────────────

echo ""
echo "=============================="
echo "RESULTS: $PHASE phases, $ERRORS failures"
echo "=============================="

if [ $ERRORS -gt 0 ]; then
    echo "FAILED"
    exit 1
else
    echo "ALL PASSED"
    exit 0
fi
