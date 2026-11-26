#!/usr/bin/env bash
# ci-golden.sh — the `stormblock` golden's recipe, run and checked (#755).
#
#   sc-build 'bash ci-golden.sh'
#
# What stormcos's deploy/test/stage-build.sh checked of the staged golden,
# moved here with the recipe (stormcentral#1836): the tree
# `deploy/build-golden.sh stormblock OUT` writes (what stormcentral's `tree`
# build runs) has every mount point the 00-stormblock unit binds over
# (stormpump#102), carries the dist build byte for byte (#209, stormcos#169),
# and its own binary on its own config serves TLS on :9090 and the boot port
# as a node needs (deploy/test/engine-tls.sh). Then the recipe's refusals.
set -euo pipefail
root=$(cd "$(dirname "$0")" && pwd)
cd "$root"
mkdir -p tmp
export TMPDIR="$root/tmp"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
fail() { echo "FAIL: ci-golden: $*" >&2; exit 1; }
OUT=$(mktemp -d "$TMPDIR/golden.XXXXXX")
trap 'rm -rf "$OUT"' EXIT

echo "== ci-golden: deploy/build-golden.sh stormblock OUT"
bash deploy/build-golden.sh stormblock "$OUT"

for d in usr/bin etc/stormblock etc/stormcert srv/registry data dev proc sys run/stormblock tmp; do
    [ -d "$OUT/$d" ] || fail "no /$d mount point in the tree (stormpump#102)"
done
dist="$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/dist/stormblock"
[ -x "$dist" ] || fail "no dist build at $dist (#209)"
[ "$(sha256sum < "$OUT/usr/bin/stormblock")" = "$(sha256sum < "$dist")" ] || fail "the tree's binary is not the dist build"
[ "$(stat -c %a "$OUT/usr/bin/stormblock")" = 755 ] || fail "usr/bin/stormblock is not 0755"
extra=$(cd "$OUT" && find . -mindepth 1 -type f ! -path ./usr/bin/stormblock ! -path ./etc/stormblock/stormblock.toml)
[ -z "$extra" ] || fail "files the stage never shipped: $extra"
echo "   tree: the mount points, the dist binary ($(stat -c %s "$dist") bytes), the config, nothing else"

echo "== ci-golden: engine TLS on the tree's binary and config"
bash deploy/test/engine-tls.sh "$OUT/usr/bin/stormblock" "$OUT/etc/stormblock/stormblock.toml"

echo "== ci-golden: refusals"
rc=0; GOLDEN_ARCH=aarch64 bash deploy/build-golden.sh stormblock "$TMPDIR/no" 2>/dev/null || rc=$?
[ "$rc" = 2 ] || fail "GOLDEN_ARCH=aarch64: exit $rc, want 2"
rc=0; bash deploy/build-golden.sh registry "$TMPDIR/no" 2>/dev/null || rc=$?
[ "$rc" = 2 ] || fail "another name: exit $rc, want 2"
echo "ci-golden: PASS"
