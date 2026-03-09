#!/usr/bin/env bash
#
# build-stormbase-iso.sh — Build a StormBase ISO with StormBlock + StormFS baked in
#
# This script:
#   1. Builds the stormblock container image and pushes to registry
#   2. Builds the stormfs container image and pushes to registry
#   3. Invokes the StormBase edition build to produce a bootable ISO
#
# The edition TOML pulls both images from registry-stormbase.gt.lo:5000.
#
# Prerequisites (build server: server1.g10.lo):
#   - Rust toolchain with x86_64-unknown-linux-musl target
#   - podman (container builds + push)
#   - skopeo (OCI image manipulation, used by build-edition.sh)
#   - xorriso, mtools, syslinux (ISO assembly)
#
# Usage:
#   ./scripts/build-stormbase-iso.sh
#
# Environment:
#   STORMBASE_DIR — path to stormbase checkout (default: ../stormbase)
#   STORMFS_DIR   — path to stormfs checkout (default: ../stormfs)
#   REGISTRY      — container registry (default: registry-stormbase.gt.lo:5000)
#   ARCH          — target architecture (default: x86_64)
#   SKIP_BUILD    — set to 1 to skip container builds (use images already in registry)
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
STORMBLOCK_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
STORMBASE_DIR="${STORMBASE_DIR:-$(cd "${STORMBLOCK_ROOT}/../stormbase" && pwd)}"
STORMFS_DIR="${STORMFS_DIR:-$(cd "${STORMBLOCK_ROOT}/../stormfs" && pwd)}"
REGISTRY="${REGISTRY:-registry-stormbase.gt.lo:5000}"
ARCH="${ARCH:-x86_64}"
SKIP_BUILD="${SKIP_BUILD:-0}"

EDITION_FILE="${STORMBASE_DIR}/editions/stormblock-server.toml"

echo "=== StormBase + StormBlock + StormFS ISO Builder ==="
echo "    StormBlock: ${STORMBLOCK_ROOT}"
echo "    StormFS:    ${STORMFS_DIR}"
echo "    StormBase:  ${STORMBASE_DIR}"
echo "    Registry:   ${REGISTRY}"
echo "    Arch:       ${ARCH}"
echo ""

# ── Validate paths ──────────────────────────────────────
if [ ! -d "${STORMBASE_DIR}" ]; then
    echo "ERROR: StormBase directory not found at ${STORMBASE_DIR}"
    echo "       Set STORMBASE_DIR to your stormbase checkout"
    exit 1
fi

if [ ! -f "${EDITION_FILE}" ]; then
    echo "ERROR: Edition file not found at ${EDITION_FILE}"
    exit 1
fi

if [ ! -d "${STORMFS_DIR}" ]; then
    echo "ERROR: StormFS directory not found at ${STORMFS_DIR}"
    echo "       Set STORMFS_DIR to your stormfs checkout"
    exit 1
fi

# ── Step 1: Build and push stormblock container ─────────
if [ "${SKIP_BUILD}" = "1" ]; then
    echo ">>> Skipping container builds (SKIP_BUILD=1)"
else
    echo ">>> Building stormblock container image..."
    podman build -t "${REGISTRY}/stormblock:latest" "${STORMBLOCK_ROOT}"
    echo ">>> Pushing stormblock to ${REGISTRY}..."
    podman push --tls-verify=false "${REGISTRY}/stormblock:latest"
    echo "    stormblock pushed"

    # ── Step 2: Build and push stormfs container ────────
    echo ""
    echo ">>> Building stormfs container image..."
    if [ ! -f "${STORMFS_DIR}/Dockerfile" ]; then
        echo "ERROR: No Dockerfile found in ${STORMFS_DIR}"
        echo "       StormFS must have a Dockerfile to build its container"
        exit 1
    fi
    podman build -t "${REGISTRY}/stormfs:latest" "${STORMFS_DIR}"
    echo ">>> Pushing stormfs to ${REGISTRY}..."
    podman push --tls-verify=false "${REGISTRY}/stormfs:latest"
    echo "    stormfs pushed"
fi

# ── Step 3: Build the edition ISO ───────────────────────
echo ""
echo ">>> Building StormBase edition ISO..."
cd "${STORMBASE_DIR}/build"
make edition EDITION="${EDITION_FILE}" ARCH="${ARCH}"

# ── Report result ───────────────────────────────────────
echo ""
echo "=== Build complete ==="
ISO_PATTERN="${STORMBASE_DIR}/build/out/stormbase-*-${ARCH}.iso"
# shellcheck disable=SC2086
ISO_FILE=$(ls -t ${ISO_PATTERN} 2>/dev/null | head -1)
if [ -n "${ISO_FILE}" ]; then
    echo "    ISO: ${ISO_FILE}"
    echo "    Size: $(ls -lh "${ISO_FILE}" | awk '{print $5}')"
else
    echo "    WARNING: ISO file not found at ${ISO_PATTERN}"
    echo "    Check the build output above for errors"
fi
