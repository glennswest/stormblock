#!/bin/bash
set -euo pipefail

echo "=== StormBlock External iSCSI Test ==="
echo "Host: $(hostname 2>/dev/null || echo container)"
echo "Date: $(date)"
echo ""

ISCSI_PORTAL="${ISCSI_PORTAL:-192.168.10.1}"
ISCSI_PORT="${ISCSI_PORT:-3260}"
ISCSI_IQN="${ISCSI_IQN:-iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-test1-raw}"

echo "Portal: ${ISCSI_PORTAL}:${ISCSI_PORT}"
echo "IQN:    ${ISCSI_IQN}"
echo ""

echo "=== Step 1: Build (debug) ==="
cargo build 2>&1
echo "Build OK"
echo ""

echo "=== Step 2: Run external iSCSI tests ==="
export ISCSI_PORTAL ISCSI_PORT ISCSI_IQN
cargo test --test external_iscsi -- --ignored --nocapture 2>&1
echo ""

echo "=== External iSCSI tests passed ==="
