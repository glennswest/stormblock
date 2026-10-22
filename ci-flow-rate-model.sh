#!/bin/bash
# #490: the flow-over's rate onto a disk-bound destination, measured with the
# rate model (`cli::forge_mode_tests::flow_over_rate_model`): a destination of
# one actuator at FLOW_MODEL_DEST_MBPS (60) with a FLOW_MODEL_DEST_SEEK_MS (10)
# seek, the appliance over NVMe/TCP on loopback, a foreground writer.
# Compares the window as before #490 (`STORMBLOCK_FLOW_OVERLAP=0`, 32 MiB
# runs) with its reads overlapping its writes at 32 and 64 MiB runs. Each
# line says where the copy's time went (read, write, read back).
set -eo pipefail
mkdir -p tmp && export TMPDIR=$PWD/tmp
m() {
    FLOW_MODEL_FOREGROUND=1 FLOW_MODEL_DEST_MBPS=${FLOW_MODEL_DEST_MBPS:-60} FLOW_MODEL_DEST_SEEK_MS=${FLOW_MODEL_DEST_SEEK_MS:-10} "$@" \
        cargo test --locked --release --lib cli::forge_mode_tests::flow_over_rate_model -- --ignored --nocapture 2>&1 \
        | grep -E "FLOW-MODEL|flow-over: [0-9]+ moved|panicked" | tail -2 || true
}
for i in 1 2; do
    echo "== as before (reads, then writes; 32 MiB runs)"; m env STORMBLOCK_FLOW_OVERLAP=0 STORMBLOCK_FLOW_RUN_MB=32
    echo "== overlapping, 32 MiB runs"; m env STORMBLOCK_FLOW_RUN_MB=32
    echo "== overlapping, 64 MiB runs (a rotating destination's default)"; m env
done
