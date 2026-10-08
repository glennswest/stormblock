#!/bin/sh
# ci-runtime-tests.sh — the runtime tests (tests-runtime/, #209) against the
# binary of this commit (#222). Part of the routine check:
#   sc-build 'cargo nextest run --locked && sh ci-runtime-tests.sh'
#
# They drive the built binary (STORMBLOCK_BIN) and need no root: boot-local
# (--check), the engine end to end over HTTP, the flow-over resume (#171/#172),
# image build/convert/inspect, slab volumes. The `#[ignore]`d ones need what a
# build VM does not have and are listed, not run: ublk_resize (root and
# ublk_drv, #342), external_iscsi and iscsi_blockdev (an external iSCSI
# target, #343).
set -eu
mkdir -p tmp
export TMPDIR="$PWD/tmp"
cargo build --locked --bin stormblock
STORMBLOCK_BIN="${CARGO_TARGET_DIR:-$PWD/target}/debug/stormblock"
export STORMBLOCK_BIN
"$STORMBLOCK_BIN" --version
echo "== not run here (#[ignore]: root, kernel devices or an external target):"
cargo test --locked -p stormblock-runtime-tests -- --list --ignored 2>/dev/null | grep ': test$' | sed 's/^/   /'
echo "== runtime tests"
cargo test --locked -p stormblock-runtime-tests -- --test-threads=4
