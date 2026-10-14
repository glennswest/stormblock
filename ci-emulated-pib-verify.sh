#!/bin/sh
# ci-emulated-pib-verify.sh — one of the ignored 1 PiB ext4 tests (#300) with its
# peak memory: the release test binary run directly, VmHWM sampled from /proc.
#   SC_BUILD_LARGE=1 sc-build 'sh ci-emulated-pib-verify.sh an_ext4_format_straight_onto_an_emulated_drive'
set -u
mkdir -p tmp && export TMPDIR=$PWD/tmp
bin=$(cargo test --locked --release --test it --no-run --message-format=json 2>/dev/null | grep -o '"executable":"[^"]*"' | tail -1 | cut -d'"' -f4)
[ -x "$bin" ] || { echo "no test binary"; cargo test --locked --release --test it --no-run 2>&1 | tail -20; exit 1; }
start=$(date +%s)
"$bin" "$1" --ignored --nocapture > tmp/out.log 2>&1 &
pid=$!
peak=0
while kill -0 $pid 2>/dev/null; do
  h=$(awk '/VmHWM/{print $2}' /proc/$pid/status 2>/dev/null); [ -n "$h" ] && [ "$h" -gt "$peak" ] && peak=$h
  sleep 2
done
wait $pid; rc=$?
grep -aE "formatted|check|clean|template ready|seal|panicked|test result|WARN|ERROR" tmp/out.log | tail -15
echo "PEAK_RSS_MIB $((peak/1024)) ELAPSED_S $(( $(date +%s) - start )) RC $rc"
