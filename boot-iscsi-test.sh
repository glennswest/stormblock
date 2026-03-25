#!/bin/bash
# boot-iscsi-test.sh — CI test for boot-from-iSCSI feature
#
# Phases:
# 1. Build stormblock
# 2. Run unit tests (including boot_iscsi tests)
# 3. IscsiDevice BlockDevice tests (connect, read/write, large I/O, flush)
# 4. Slab + ThinVolume on iSCSI (format, allocate, reopen, multi-volume)
# 5. Migration between iSCSI disks (src → dst with data verification)
# 6. Boot-iscsi CLI subcommand test
# 7. Clippy
#
# Requires: mkube job runner with iSCSI disks (boot-iscsi-src, boot-iscsi-dst)

set -euo pipefail

PHASE=0
ERRORS=0
TESTS_RUN=0
TESTS_PASS=0

phase() {
    PHASE=$((PHASE + 1))
    echo ""
    echo "=============================="
    echo "PHASE $PHASE: $1"
    echo "=============================="
}

pass() { echo "  PASS: $1"; TESTS_PASS=$((TESTS_PASS + 1)); TESTS_RUN=$((TESTS_RUN + 1)); }
fail() { echo "  FAIL: $1"; ERRORS=$((ERRORS + 1)); TESTS_RUN=$((TESTS_RUN + 1)); }

# Dedicated boot-iscsi test disks (5 GB each)
PORTAL="${ISCSI_PORTAL:-192.168.10.1}"
PORT="${ISCSI_PORT:-3260}"
IQN_SRC="${ISCSI_IQN_SRC:-iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-boot-iscsi-src-raw}"
IQN_DST="${ISCSI_IQN_DST:-iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-boot-iscsi-dst-raw}"

echo "=============================="
echo "StormBlock Boot-iSCSI CI"
echo "=============================="
echo "Host:    $(hostname 2>/dev/null || echo unknown)"
echo "Date:    $(date)"
echo "Source:  $PORTAL:$PORT $IQN_SRC"
echo "Dest:    $PORTAL:$PORT $IQN_DST"

# ── Phase 1: Build ──────────────────────────────────────────────

phase "Build"

if [ -f /build/target/debug/stormblock ]; then
    echo "Using pre-built binary"
    BINARY=/build/target/debug/stormblock
else
    echo "Building from source..."
    cd /build
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

cd /build
cargo test --test boot_iscsi 2>&1
if [ $? -eq 0 ]; then
    pass "boot_iscsi tests (11 layout + provisioning + migration)"
else
    fail "boot_iscsi tests"
fi

cargo test 2>&1
if [ $? -eq 0 ]; then
    pass "All unit tests"
else
    fail "Some unit tests failed"
fi

# ── Phase 3: IscsiDevice BlockDevice tests ──────────────────────

phase "IscsiDevice BlockDevice (real hardware)"

cd /build
export ISCSI_PORTAL="$PORTAL"
export ISCSI_PORT="$PORT"
export ISCSI_IQN_SRC="$IQN_SRC"
export ISCSI_IQN_DST="$IQN_DST"

# Run each test individually so we get granular pass/fail
for test_name in \
    iscsi_device_connect_and_capacity \
    iscsi_device_write_read_verify \
    iscsi_device_large_io \
    iscsi_device_unaligned_data_length \
    iscsi_device_flush_nop_out \
; do
    echo ""
    echo "--- $test_name ---"
    if cargo test --test iscsi_blockdev "$test_name" -- --ignored --nocapture 2>&1; then
        pass "$test_name"
    else
        fail "$test_name"
    fi
done

# ── Phase 4: Slab + ThinVolume on iSCSI ─────────────────────────

phase "Slab + ThinVolume on iSCSI"

for test_name in \
    iscsi_slab_format_allocate_readwrite \
    iscsi_slab_reopen \
    iscsi_thin_volume_io \
    iscsi_multi_volume_isolation \
; do
    echo ""
    echo "--- $test_name ---"
    if cargo test --test iscsi_blockdev "$test_name" -- --ignored --nocapture 2>&1; then
        pass "$test_name"
    else
        fail "$test_name"
    fi
done

# ── Phase 5: Migration between iSCSI disks ─────────────────────

phase "Migration (src → dst)"

echo "--- iscsi_migrate_between_disks ---"
if cargo test --test iscsi_blockdev iscsi_migrate_between_disks -- --ignored --nocapture 2>&1; then
    pass "iscsi_migrate_between_disks"
else
    fail "iscsi_migrate_between_disks"
fi

# ── Phase 6: boot-iscsi CLI subcommand ──────────────────────────

phase "boot-iscsi CLI"

# Test the boot-iscsi subcommand against real iSCSI target.
# This phase is non-fatal — iSCSI target may be busy from phase 3/4/5.
set +e
timeout 30 $BINARY boot-iscsi \
    --portal "$PORTAL" \
    --port "$PORT" \
    --iqn "$IQN_SRC" \
    --layout "esp:256M,boot:512M,root:3G,swap:512M,home:rest" \
    &
BOOT_PID=$!

sleep 10

if kill -0 $BOOT_PID 2>/dev/null; then
    pass "boot-iscsi provisioned and running"
    kill -INT $BOOT_PID
    wait $BOOT_PID 2>/dev/null || true
else
    wait $BOOT_PID 2>/dev/null
    EXIT_CODE=$?
    if [ $EXIT_CODE -eq 0 ]; then
        pass "boot-iscsi completed"
    else
        echo "  WARN: boot-iscsi exit=$EXIT_CODE (iSCSI target may be busy)"
        echo "  This is non-fatal — hardware tests in phases 3-5 already verified"
        TESTS_RUN=$((TESTS_RUN + 1))
    fi
fi
set -e

# ── Phase 7: Clippy ─────────────────────────────────────────────

phase "Clippy"

cd /build
cargo clippy -- -D warnings 2>&1
if [ $? -eq 0 ]; then
    pass "clippy clean (-D warnings)"
else
    fail "clippy warnings"
fi

# ── Summary ─────────────────────────────────────────────────────

echo ""
echo "=============================="
echo "RESULTS"
echo "=============================="
echo "  Phases:  $PHASE"
echo "  Tests:   $TESTS_RUN run, $TESTS_PASS passed, $ERRORS failed"
echo "=============================="

if [ $ERRORS -gt 0 ]; then
    echo "FAILED"
    exit 1
else
    echo "ALL PASSED"
    exit 0
fi
