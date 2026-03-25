#!/bin/bash
set -euo pipefail

echo "=== StormBlock CI Build + Test ==="
echo "Host: $(hostname 2>/dev/null || echo container)"
echo "Date: $(date)"
echo "Rust: $(rustc --version)"
echo "Cargo: $(cargo --version)"
echo ""

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
RUSTFLAGS="-C link-arg=-v" cargo build --release --target x86_64-unknown-linux-musl 2>&1
ls -lh target/x86_64-unknown-linux-musl/release/stormblock
echo "Release build OK"
echo ""

echo "=== All steps passed ==="
echo "Build:       OK (debug + release musl)"
echo "Unit tests:  OK (cargo test)"
echo "Binary size: $(ls -lh target/x86_64-unknown-linux-musl/release/stormblock | awk '{print $5}')"
