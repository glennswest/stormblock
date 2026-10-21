#!/usr/bin/env bash
# The engine's NVMe/TCP Identify bytes are those of a base revision (#474).
#
# #474 moved the wire half of the engine's target (PDUs, fabric, discovery,
# DH-HMAC-CHAP, Identify) into crates/nvmeof-target. pdu, fabric, discovery
# and auth moved file-for-file; admin's Identify Controller/Namespace, the
# namespace descriptor list and the namespace lists are now built by the
# crate. This builds one dump program against BASE (default 8f23f72, the
# commit before the move) and against this checkout, and compares every byte
# each answers for a spread of inputs.
#   sc-build 'bash ci-nvmeof-identify-same.sh [BASE]'
set -uo pipefail
BASE=${1:-8f23f721689d3fcfbc534bbd51ea9cdd2cadf174}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"; export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/identify.XXXXXX")
trap 'git -C "$ROOT" worktree remove --force "$W/base" >/dev/null 2>&1; rm -rf "$W"' EXIT

cat > "$W/identify_dump.rs" <<'RS'
//! Every Identify answer the engine gives, for a spread of inputs (#474).
use std::sync::Arc;
use stormblock::drive::filedev::FileDevice;
use stormblock::drive::BlockDevice;
use stormblock::target::nvmeof::admin::*;

fn show(what: &str, d: &[u8]) {
    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
    println!("{what} {hex}");
}

#[tokio::main]
async fn main() {
    let dir = std::env::args().nth(1).unwrap();
    for (nqn, sn, mn, fw, nn) in [
        ("nqn.2024.io.stormblock:target", "SB0001", "StormBlock", "21.0", 1024u32),
        ("nqn.2026-09.lo.storm:host-server8", "a-serial-longer-than-twenty-bytes", "a model name that is longer than forty bytes in all", "firmware9", 1),
        ("", "", "", "", 0),
    ] {
        for disc in [false, true] {
            show(&format!("controller {nqn:?} {disc}"), &identify_controller(nqn, sn, mn, fw, nn, disc));
        }
    }
    for (i, cap) in [4096u64, 1 << 20, 64 << 20, (1 << 30) + 4096].iter().enumerate() {
        let p = format!("{dir}/dev{i}");
        let d: Arc<dyn BlockDevice> = Arc::new(FileDevice::open_with_capacity(&p, *cap).await.unwrap());
        show(&format!("namespace {cap}"), &identify_namespace(&d));
        show(&format!("desc {cap}"), &identify_ns_desc_list(d.id().uuid.as_bytes()));
    }
    show("desc zero", &identify_ns_desc_list(&[0; 16]));
    show("desc ff", &identify_ns_desc_list(&[0xff; 16]));
    let many: Vec<u32> = (1..=1100).collect();
    for (what, l) in [("none", &[][..]), ("some", &[1u32, 7, 9][..]), ("1024", &many[..1024]), ("1100", &many[..])] {
        show(&format!("changed {what}"), &changed_ns_list(l, false));
        show(&format!("changed {what} overflow"), &changed_ns_list(l, true));
        show(&format!("active {what}"), &active_ns_list(l));
    }
    println!("mdts {:?}", [0usize, 4096, 128 << 10, 8 << 20, 32 << 20].map(stormblock::target::nvmeof::mdts_for));
}
RS

dump() { # tree out
    mkdir -p "$1/examples"
    cp "$W/identify_dump.rs" "$1/examples/identify_dump.rs"
    # One directory for both: a file device's id (the NGUID) is its path's.
    rm -rf "$W/devs"; mkdir -p "$W/devs"
    (cd "$1" && cargo run --quiet --locked --example identify_dump -- "$W/devs") > "$2" || return 1
}
# A build VM's clone holds the pushed commit alone: fetch the base by its
# full id.
REPO=${STORMBLOCK_REPO:-$(git -C "$ROOT" remote get-url origin 2>/dev/null || echo https://github.com/glennswest/stormblock)}
git -C "$ROOT" cat-file -e "$BASE^{commit}" 2>/dev/null || git -C "$ROOT" fetch -q "$REPO" "$BASE" \
    || { echo "FAIL: cannot fetch $BASE (give its full id)"; exit 1; }
git -C "$ROOT" worktree add --detach "$W/base" "$BASE" >/dev/null 2>&1 || { echo "FAIL: no worktree at $BASE"; exit 1; }
echo "== $BASE"; dump "$W/base" "$W/base.txt" || { echo "FAIL: the dump at $BASE"; exit 1; }
echo "== $(git -C "$ROOT" rev-parse --short HEAD)"; dump "$ROOT" "$W/head.txt" || { rm -f "$ROOT/examples/identify_dump.rs"; echo "FAIL: the dump here"; exit 1; }
rm -f "$ROOT/examples/identify_dump.rs"
n=$(wc -l < "$W/head.txt")
if cmp -s "$W/base.txt" "$W/head.txt"; then
    echo "  ok    $n Identify answers, byte for byte the same as $BASE"
    echo "ALL PASS"
else
    diff <(cut -c1-120 "$W/base.txt") <(cut -c1-120 "$W/head.txt") | head -20
    echo "FAIL: the Identify bytes differ from $BASE"; exit 1
fi
