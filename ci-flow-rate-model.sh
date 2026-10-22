#!/bin/bash
# #490: the flow-over's rate onto a disk-bound destination, measured with the
# rate model (`cli::forge_mode_tests::flow_over_rate_model`): a destination of
# one actuator at FLOW_MODEL_DEST_MBPS (60) with a FLOW_MODEL_DEST_SEEK_MS (10)
# seek, the appliance over NVMe/TCP on loopback, a foreground writer.
# Compares a flow-over that reads each copy back as it writes it with one
# that leaves that to #412's validation (the default since #490's A). Each
# line says where the copy's time went (read, write, read back) and the
# validation's own rate.
set -eo pipefail
mkdir -p tmp && export TMPDIR=$PWD/tmp
m() {
    FLOW_MODEL_FOREGROUND=1 FLOW_MODEL_DEST_MBPS=${FLOW_MODEL_DEST_MBPS:-60} FLOW_MODEL_DEST_SEEK_MS=${FLOW_MODEL_DEST_SEEK_MS:-10} "$@" \
        cargo test --locked --release --lib cli::forge_mode_tests::flow_over_rate_model -- --ignored --nocapture 2>&1 \
        | grep -E "FLOW-MODEL|flow-over: [0-9]+ moved|panicked" | tail -2 || true
}
for i in 1 2; do
    echo "== each copy read back (STORMBLOCK_FLOW_READBACK=1, as before #490's A)"; m env STORMBLOCK_FLOW_READBACK=1
    echo "== no read-back; #412's validation checks every moved extent (the default)"; m env
done
