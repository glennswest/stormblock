#!/usr/bin/env bash
# stormboot-serve builds for the owner's Mac and Windows machines (#474).
#
# The crate is tokio and plain std file I/O only, so it must build for every
# desktop a laptop runs. This builds `stormboot-serve` for x86_64 and aarch64
# macOS (cargo zigbuild) and x86_64 Windows (windows-gnu), checks what came
# out (`file`: Mach-O, PE32+), and keeps the binaries' sizes and sha256 in
# the output. The engine itself is never built for them.
#   sc-build 'bash ci-bootserve-cross.sh'
set -uo pipefail
mkdir -p tmp; export TMPDIR=${TMPDIR:-$PWD/tmp}
FAILS=0
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }
command -v cargo-zigbuild >/dev/null 2>&1 || cargo install cargo-zigbuild --locked --quiet \
    || { echo "SKIP: no cargo-zigbuild"; exit 2; }
command -v zig >/dev/null 2>&1 || { echo "SKIP: no zig"; exit 2; }
OUT="${CARGO_TARGET_DIR:-target}"
for t in x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-gnu; do
    rustup target add "$t" >/dev/null 2>&1 || true
    echo "== $t"
    if cargo zigbuild --release --locked -p stormblock-nvmeof --bin stormboot-serve --target "$t" > "${TMPDIR:-/tmp}/cross.out" 2>&1; then
        # Zero warnings, on every target (stormcentral#193). One is the build
        # VM's, not this code's: no macOS SDK there (stormcentral#1077), so
        # rustc says it could not ask xcrun for one. Named, not counted.
        c="${TMPDIR:-/tmp}/cross.out"
        if grep -q 'xcrun.*to find MacOSX.sdk failed' "$c"; then
            echo "  note  $t: the build VM has no macOS SDK (stormcentral#1077); zig's libSystem stubs linked it"
            # The note and the summary line that counts it.
            sed -i '/^warning: invoking `"xcrun"/,/^$/d; /generated 1 warning$/d' "$c"
        fi
        if grep -q '^warning' "$c"; then
            grep -A12 '^warning' "$c" | head -40
            fail "$t: the build warns"
        fi
        b="$OUT/$t/release/stormboot-serve"
        case "$t" in *windows*) b="$b.exe" ;; esac
        if [ -f "$b" ]; then
            kind=$(file -b "$b")
            case "$t:$kind" in
                *apple*:Mach-O*|*windows*:PE32+*) ok "$t: $(stat -c %s "$b") bytes, sha256 $(sha256sum "$b" | cut -c1-16), $kind" ;;
                *) fail "$t: not the right kind of binary: $kind" ;;
            esac
        else
            fail "$t: no $b"
        fi
    else
        tail -30 "${TMPDIR:-/tmp}/cross.out"
        fail "$t: the build"
    fi
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
