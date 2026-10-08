#!/bin/sh
# check.sh — the routine check of a stormblock commit (#357).
#
#   sc-build 'sh check.sh'
#
# What a commit must pass before it is called done, in one command, so no part
# of it is skipped by accident:
#   1. the test image (test/build.sh): the musl release build of `stormblock`
#      and `stormblock-test`, packaged FROM scratch. 87d2d99 reached main
#      compiling neither its tests nor this image; the test machines found it.
#   2. every initramfs shell test, under sh and busybox sh where it is there.
#   3. the whole suite with cargo-nextest (installed when the build VM has
#      none, stormcentral#534).
#   4. the runtime tests against this commit's binary (ci-runtime-tests.sh).
# Stops at the first stage that fails. CHECK_SKIP_IMAGE=1 leaves out the
# podman packaging (stage only), for a box without podman.
set -eu
root=$(cd "$(dirname "$0")" && pwd)
cd "$root"
mkdir -p tmp
export TMPDIR="$root/tmp"

echo "== check: the test image (test/build.sh)"
if [ "${CHECK_SKIP_IMAGE:-0}" = 1 ] || ! command -v podman >/dev/null 2>&1; then
    STAGE_ONLY=1 sh test/build.sh
else
    sh test/build.sh
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
