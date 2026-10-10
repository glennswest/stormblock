#!/bin/sh
# ci-parity-aarch64.sh — the NEON half of src/raid/parity.rs (#255), tested on
# an x86 build VM: the module is self-contained (std only), so it is built
# alone as an aarch64 musl test binary, linked by rust-lld, and run under
# qemu-user. It runs every parity test, `every_simd_level_matches_the_
# portable_code` comparing NEON with the portable code.
#
#   sc-build 'sh ci-parity-aarch64.sh'
set -u
mkdir -p tmp
QEMU=$(command -v qemu-aarch64 || command -v qemu-aarch64-static || true)
[ -n "$QEMU" ] || { echo "SKIP: no qemu-aarch64 (qemu-user)"; exit 2; }
rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1 || { echo "SKIP: cannot add the aarch64 target"; exit 2; }
rustc --edition 2021 --test -O --target aarch64-unknown-linux-musl \
    -C linker=rust-lld -C link-self-contained=yes \
    src/raid/parity.rs -o tmp/parity-aarch64 || { echo "FAIL: build"; exit 1; }
"$QEMU" tmp/parity-aarch64 --nocapture 2>&1 | tail -25
