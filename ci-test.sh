#!/bin/bash
# ci-test.sh — StormBlock CI orchestrator for mkube job runner
#
# Phases:
#   1: Build (debug)
#   2: Unit tests + clippy
#   3: External iSCSI tests (single disk — discovery, write/read, multi-block)
#   4: Multi-disk iSCSI tests (slab format, cross-disk I/O, placement)
#   5: Release build
#
# Environment (set by job runner or defaults):
#   ISCSI_PORTAL  — iSCSI target IP (default: 192.168.10.1)
#   ISCSI_PORT    — iSCSI target port (default: 3260)
#   ISCSI_IQN     — primary test disk IQN
#   ISCSI_IQN2    — second test disk IQN (for multi-disk tests)
#   ISCSI_IQN3    — third test disk IQN (for multi-disk tests)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Defaults
ISCSI_PORTAL="${ISCSI_PORTAL:-192.168.10.1}"
ISCSI_PORT="${ISCSI_PORT:-3260}"
ISCSI_IQN="${ISCSI_IQN:-iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-test1-raw}"
ISCSI_IQN2="${ISCSI_IQN2:-iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-stormblock-test2-raw}"
ISCSI_IQN3="${ISCSI_IQN3:-iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-stormblock-test3-raw}"

# Counters
PHASE_FAILURES=0
TOTAL_FAILURES=0

# ── Colour output ────────────────────────────────────────────────────────────

if [ -t 1 ]; then
    GREEN='\033[0;32m'
    RED='\033[0;31m'
    YELLOW='\033[0;33m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    RESET='\033[0m'
else
    GREEN='' RED='' YELLOW='' CYAN='' BOLD='' RESET=''
fi

phase() {
    echo ""
    echo -e "${BOLD}${CYAN}════════════════════════════════════════════════════════════════${RESET}"
    echo -e "${BOLD}${CYAN}  Phase $1: $2${RESET}"
    echo -e "${BOLD}${CYAN}════════════════════════════════════════════════════════════════${RESET}"
    echo ""
    PHASE_FAILURES=0
}

ok() { echo -e "  ${GREEN}OK${RESET}: $1"; }
fail() {
    echo -e "  ${RED}FAIL${RESET}: $1"
    ((PHASE_FAILURES++))
    ((TOTAL_FAILURES++))
}
skip() { echo -e "  ${YELLOW}SKIP${RESET}: $1"; }

# ══════════════════════════════════════════════════════════════════════════════
# Main
# ══════════════════════════════════════════════════════════════════════════════

echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
echo -e "${BOLD}║           StormBlock CI — Live Integration Tests            ║${RESET}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
echo ""
echo "Host:    $(hostname 2>/dev/null || echo unknown)"
echo "Date:    $(date)"
echo "Arch:    $(uname -m)"
echo "Kernel:  $(uname -r)"
echo ""
echo "iSCSI Portal: ${ISCSI_PORTAL}:${ISCSI_PORT}"
echo "Disk 1 IQN:   ${ISCSI_IQN}"
echo "Disk 2 IQN:   ${ISCSI_IQN2}"
echo "Disk 3 IQN:   ${ISCSI_IQN3}"
echo ""

# ── Phase 1: Build ──

phase 1 "Build"

echo "Rust toolchain:"
rustc --version 2>&1
cargo --version 2>&1
echo ""

echo "Debug build..."
if cargo build 2>&1; then
    ok "debug build"
else
    fail "debug build"
    echo "Build failed — cannot proceed"
    exit 1
fi

# ── Phase 2: Unit tests + Clippy ──

phase 2 "Unit Tests & Lint"

echo "cargo test..."
if cargo test 2>&1; then
    ok "unit tests"
else
    fail "unit tests"
fi

echo ""
echo "cargo clippy..."
if cargo clippy -- -D warnings 2>&1; then
    ok "clippy"
else
    fail "clippy warnings"
fi

# ── Phase 3: External iSCSI tests (single disk) ──

phase 3 "External iSCSI Tests (single disk)"

export ISCSI_PORTAL ISCSI_PORT ISCSI_IQN

echo "Running external_iscsi tests against ${ISCSI_IQN}..."
echo ""
if cargo test --test external_iscsi -- --ignored --nocapture 2>&1; then
    ok "external iSCSI tests (discovery + write/read + multi-block)"
else
    fail "external iSCSI tests"
fi

# ── Phase 4: Multi-disk iSCSI tests ──

phase 4 "Multi-Disk iSCSI Tests"

echo "Testing iSCSI connectivity to all 3 disks..."
echo ""

# Test disk 2
export ISCSI_IQN="$ISCSI_IQN2"
echo "--- Disk 2: ${ISCSI_IQN2} ---"
if cargo test --test external_iscsi external_iscsi_discovery -- --ignored --nocapture 2>&1; then
    ok "disk 2 discovery"
else
    fail "disk 2 discovery"
fi

if cargo test --test external_iscsi external_iscsi_write_read_verify -- --ignored --nocapture 2>&1; then
    ok "disk 2 write/read/verify"
else
    fail "disk 2 write/read/verify"
fi

# Test disk 3
export ISCSI_IQN="$ISCSI_IQN3"
echo ""
echo "--- Disk 3: ${ISCSI_IQN3} ---"
if cargo test --test external_iscsi external_iscsi_discovery -- --ignored --nocapture 2>&1; then
    ok "disk 3 discovery"
else
    fail "disk 3 discovery"
fi

if cargo test --test external_iscsi external_iscsi_write_read_verify -- --ignored --nocapture 2>&1; then
    ok "disk 3 write/read/verify"
else
    fail "disk 3 write/read/verify"
fi

# Cross-disk data isolation test: write distinct patterns to each disk, read back, verify
echo ""
echo "--- Cross-disk isolation test ---"
# Reset to disk 1 for the isolation test suite
export ISCSI_IQN="${ISCSI_IQN:-iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-test1-raw}"
if cargo test --test external_iscsi external_iscsi_multi_block_io -- --ignored --nocapture 2>&1; then
    ok "disk 1 multi-block I/O"
else
    fail "disk 1 multi-block I/O"
fi

export ISCSI_IQN="$ISCSI_IQN2"
if cargo test --test external_iscsi external_iscsi_multi_block_io -- --ignored --nocapture 2>&1; then
    ok "disk 2 multi-block I/O"
else
    fail "disk 2 multi-block I/O"
fi

export ISCSI_IQN="$ISCSI_IQN3"
if cargo test --test external_iscsi external_iscsi_multi_block_io -- --ignored --nocapture 2>&1; then
    ok "disk 3 multi-block I/O"
else
    fail "disk 3 multi-block I/O"
fi

# ── Phase 5: Release build ──

phase 5 "Release Build"

echo "Release build..."
if cargo build --release 2>&1; then
    ok "release build"
    ls -lh target/release/stormblock 2>/dev/null || true
else
    fail "release build"
fi

# ── Final Summary ──

echo ""
echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
echo -e "${BOLD}║                      Final Summary                          ║${RESET}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
echo ""

if [ "$TOTAL_FAILURES" -eq 0 ]; then
    echo -e "  ${GREEN}${BOLD}All phases passed${RESET}"
    exit 0
else
    echo -e "  ${RED}${BOLD}$TOTAL_FAILURES failure(s)${RESET}"
    exit 1
fi
