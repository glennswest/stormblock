set -eo pipefail
mkdir -p tmp && export TMPDIR=$PWD/tmp
m() { FLOW_MODEL_FOREGROUND=1 FLOW_MODEL_DEST_MBPS=60 FLOW_MODEL_DEST_SEEK_MS=10 "$@" cargo test --locked --release --lib cli::forge_mode_tests::flow_over_rate_model -- --ignored --nocapture 2>&1 | grep -E "FLOW-MODEL|flow-over: [0-9]+ moved|panicked" | tail -2 || true; }
for i in 1 2; do
echo "== old (no overlap, 32 MiB)"; m env STORMBLOCK_FLOW_OVERLAP=0 STORMBLOCK_FLOW_RUN_MB=32
echo "== overlap, 32 MiB"; m env STORMBLOCK_FLOW_RUN_MB=32
echo "== overlap, 64 MiB (default on a rotating disk)"; m env
done
