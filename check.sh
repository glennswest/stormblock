#!/bin/sh
# check.sh — the routine check of a stormblock commit (#357).
#
#   sc-build 'sh check.sh'
#
# What a commit must pass before it is called done, in one command, so no part
# of it is skipped by accident:
#   1. the musl release build of `stormblock`, the static binary nodes and
#      stormblock-test's image run. 87d2d99 reached main not compiling; the
#      test machines found it. The suites themselves live in
#      glennswest/stormblock-test (#371): with STORMBLOCK_TEST_DIR set to a
#      checkout of it, its own check (test/check.sh, `short`) runs against
#      this checkout too.
#   2. every initramfs shell test, under sh and busybox sh where it is there.
#   3. the whole suite with cargo-nextest (installed when the build VM has
#      none, stormcentral#534).
#   4. the runtime tests against this commit's binary (ci-runtime-tests.sh).
# Stops at the first stage that fails.
set -eu
root=$(cd "$(dirname "$0")" && pwd)
cd "$root"
mkdir -p tmp
export TMPDIR="$root/tmp"

echo "== check: the musl release build"
command -v rustup >/dev/null 2>&1 && rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || true
cargo build --release --locked --target x86_64-unknown-linux-musl -p stormblock
if [ -n "${STORMBLOCK_TEST_DIR:-}" ]; then
    echo "== check: stormblock-test's short suite against this checkout"
    STORM_COMPONENT_DIR="$root" sh "$STORMBLOCK_TEST_DIR/test/check.sh"
fi

echo "== check: initramfs tests"
shells=sh
command -v busybox >/dev/null 2>&1 && shells="sh busybox_sh"
for t in tests/initramfs-*.sh; do
    for s in $shells; do
        if [ "$s" = busybox_sh ]; then busybox sh "$t" >"$TMPDIR/check.out" 2>&1
        else sh "$t" >"$TMPDIR/check.out" 2>&1; fi || {
            cat "$TMPDIR/check.out"; echo "FAIL: $t under $s"; exit 1; }
    done
    echo "  ok    $t ($shells)"
done

echo "== check: cargo nextest"
command -v cargo-nextest >/dev/null 2>&1 || cargo install cargo-nextest --locked --quiet
cargo nextest run --locked --no-fail-fast

echo "== check: runtime tests"
sh ci-runtime-tests.sh
echo "== check: ALL PASS"
