#!/bin/bash
# ci-nvmeof-hotadd.sh — Linux verification for the registry-scale export path.
#
# Phases:
#   1: Toolchain + debug build
#   2: Full test suite (Linux — exercises the cfg(linux) paths macOS skips)
#   3: Clippy
#   4: Release build
#   5: Live NVMe-oF hot-add against the real kernel nvme_tcp initiator
#
# Phase 5 is the one that matters: everything else about hot-add is proven
# only by our own unit tests. It connects ONCE, then attaches a second volume
# and asserts the new namespace appears on the already-connected controller —
# no reconnect. That is the whole premise of the feature.
#
# Phase 5 skips (rather than fails) when the container cannot do NVMe: no
# nvme-cli, no nvme_tcp module, or not root. A skip is reported loudly so a
# green run is never mistaken for "hot-add verified".

set -uo pipefail

TOTAL_FAILURES=0
SKIPPED=0
declare -a PHASE_RESULTS=()

GREEN='' RED='' YELLOW='' CYAN='' BOLD='' RESET=''
if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'
    CYAN='\033[0;36m'; BOLD='\033[1m'; RESET='\033[0m'
fi

CUR_PHASE=""
CUR_PHASE_FAILS=0

phase() {
    if [ -n "$CUR_PHASE" ]; then
        if [ "$CUR_PHASE_FAILS" -eq 0 ]; then
            PHASE_RESULTS+=("PASS: $CUR_PHASE")
        else
            PHASE_RESULTS+=("FAIL: $CUR_PHASE ($CUR_PHASE_FAILS failures)")
        fi
    fi
    CUR_PHASE="Phase $1: $2"
    CUR_PHASE_FAILS=0
    echo ""
    echo -e "${BOLD}${CYAN}════════════════════════════════════════════════════════════${RESET}"
    echo -e "${BOLD}${CYAN}  Phase $1: $2${RESET}"
    echo -e "${BOLD}${CYAN}════════════════════════════════════════════════════════════${RESET}"
    echo ""
}

ok()   { echo -e "  ${GREEN}OK${RESET}: $1"; }
fail() { echo -e "  ${RED}FAIL${RESET}: $1"; CUR_PHASE_FAILS=$((CUR_PHASE_FAILS+1)); TOTAL_FAILURES=$((TOTAL_FAILURES+1)); }
skip() { echo -e "  ${YELLOW}SKIP${RESET}: $1"; SKIPPED=$((SKIPPED+1)); }
info() { echo "  .. $1"; }

echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
echo -e "${BOLD}║      StormBlock CI — Linux suite + live NVMe-oF hot-add     ║${RESET}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
echo "Host:   $(hostname 2>/dev/null || echo unknown)"
echo "Date:   $(date)"
echo "Arch:   $(uname -m)   Kernel: $(uname -r)"
echo "User:   $(id -un) (uid $(id -u))"
echo ""

# ── Phase 1: build ──────────────────────────────────────────────────────────

phase 1 "Toolchain + debug build"
rustc --version 2>&1 || true
cargo --version 2>&1 || true
echo ""
if cargo build 2>&1; then
    ok "debug build"
else
    fail "debug build"
    echo "Cannot proceed without a binary."
    exit 1
fi

# ── Phase 2: full suite ─────────────────────────────────────────────────────

phase 2 "Full test suite (Linux)"
if cargo test 2>&1; then
    ok "cargo test"
else
    fail "cargo test"
fi

# ── Phase 3: clippy ─────────────────────────────────────────────────────────

phase 3 "Clippy"
if cargo clippy --all-targets 2>&1; then
    ok "clippy"
else
    fail "clippy"
fi

# ── Phase 4: release build ──────────────────────────────────────────────────

phase 4 "Release build"
if cargo build --release 2>&1; then
    ok "release build"
    ls -lh target/release/stormblock 2>/dev/null || true
else
    fail "release build"
fi

# ── Phase 5: live NVMe-oF hot-add ───────────────────────────────────────────

phase 5 "Live NVMe-oF hot-add (real kernel initiator)"

NQN="nqn.2024.io.stormblock:cihotadd"
PORT=4420
MGMT=9090
WORKDIR="$(mktemp -d)"
DATA_DIR="$WORKDIR/data"
NODE="ci-node"
SB_PID=""
CONNECTED=0

cleanup() {
    if [ "$CONNECTED" = "1" ]; then
        nvme disconnect -n "$NQN" >/dev/null 2>&1 || true
    fi
    if [ -n "$SB_PID" ] && kill -0 "$SB_PID" 2>/dev/null; then
        kill "$SB_PID" 2>/dev/null || true
        wait "$SB_PID" 2>/dev/null || true
    fi
    rm -rf "$WORKDIR" 2>/dev/null || true
}
trap cleanup EXIT

api() { curl -s -m 10 "$@"; }

find_ctrl() {
    for c in /sys/class/nvme/nvme*; do
        [ -r "$c/subsysnqn" ] || continue
        if [ "$(cat "$c/subsysnqn" 2>/dev/null)" = "$NQN" ]; then
            basename "$c"; return 0
        fi
    done
    return 1
}

# Namespace IDs the kernel currently sees on this controller, sorted.
list_nsids() {
    local ctrl="$1" out=""
    for n in /sys/class/nvme/"$ctrl"/"$ctrl"n*; do
        [ -r "$n/nsid" ] || continue
        out="$out $(cat "$n/nsid" 2>/dev/null)"
    done
    echo $out | tr ' ' '\n' | sort -n | tr '\n' ' ' | sed 's/ $//'
}

# Preflight — every reason we might not be able to run this.
PREFLIGHT_OK=1
if [ "$(id -u)" != "0" ]; then
    skip "not root — nvme connect needs privileges"; PREFLIGHT_OK=0
fi
if ! command -v nvme >/dev/null 2>&1; then
    skip "nvme-cli not installed in this image"; PREFLIGHT_OK=0
fi
if [ "$PREFLIGHT_OK" = "1" ]; then
    modprobe nvme_tcp 2>/dev/null || true
    modprobe nvme_fabrics 2>/dev/null || true
    if [ ! -e /dev/nvme-fabrics ]; then
        skip "/dev/nvme-fabrics absent — nvme_tcp not loadable in this container"
        PREFLIGHT_OK=0
    fi
fi

if [ "$PREFLIGHT_OK" != "1" ]; then
    echo ""
    echo -e "  ${YELLOW}Hot-add NOT verified in this run.${RESET}"
    echo "  Needs a privileged container or a VM with nvme_tcp + nvme-cli."
else
    info "preflight OK — nvme-cli present, nvme_tcp loadable, running as root"

    # Backing store: two files under a RAID1, one volume exported as NSID 1.
    mkdir -p "$DATA_DIR"
    truncate -s 256M "$WORKDIR/d1.img"
    truncate -s 256M "$WORKDIR/d2.img"
    cat > "$WORKDIR/stormblock.toml" <<EOF
[management]
listen_addr = "127.0.0.1:$MGMT"
data_dir = "$DATA_DIR"
node_name = "$NODE"
advertised_addr = "127.0.0.1"
EOF

    info "starting target..."
    STORMBLOCK_NODE="$NODE" RUST_LOG=stormblock=info \
    ./target/debug/stormblock \
        --config "$WORKDIR/stormblock.toml" \
        --device "$WORKDIR/d1.img" --device "$WORKDIR/d2.img" \
        --raid raid1 --volume golden:64M \
        --data-dir "$DATA_DIR" \
        --no-iscsi \
        --nvmeof-addr "127.0.0.1:$PORT" \
        --nvmeof-nqn "$NQN" \
        > "$WORKDIR/target.log" 2>&1 &
    SB_PID=$!

    # Wait for the management API.
    UP=0
    for _ in $(seq 1 40); do
        if api "http://127.0.0.1:$MGMT/api/v1/drives" >/dev/null 2>&1; then UP=1; break; fi
        if ! kill -0 "$SB_PID" 2>/dev/null; then break; fi
        sleep 0.5
    done
    if [ "$UP" != "1" ]; then
        fail "target did not come up"
        echo "--- target log ---"; cat "$WORKDIR/target.log" 2>/dev/null | tail -40
    else
        ok "target up (mgmt API responding)"

        # ── Connect ONCE ──
        if nvme connect -t tcp -a 127.0.0.1 -s "$PORT" -n "$NQN" >"$WORKDIR/connect.log" 2>&1; then
            CONNECTED=1
            ok "nvme connect succeeded"
        else
            fail "nvme connect failed"
            cat "$WORKDIR/connect.log" 2>/dev/null | head -20
        fi

        if [ "$CONNECTED" = "1" ]; then
            sleep 2
            CTRL="$(find_ctrl || true)"
            if [ -z "$CTRL" ]; then
                fail "connected but no controller with subsysnqn $NQN"
                ls -d /sys/class/nvme/nvme* 2>/dev/null || true
            else
                ok "controller $CTRL attached"
                BEFORE="$(list_nsids "$CTRL")"
                info "namespaces before hot-add: [$BEFORE]"
                [ -n "$BEFORE" ] && ok "boot namespace present" || fail "no namespace after connect"

                # ── Hot-add a second volume, WITHOUT reconnecting ──
                info "creating + attaching a second volume via /v1..."
                CREATE="$(api -X POST "http://127.0.0.1:$MGMT/v1/volumes" \
                    -H 'Content-Type: application/json' \
                    -d '{"name":"hotadd-1","size_bytes":33554432,"replica_tier":{"slaves":0}}')"
                VOL_ID="$(echo "$CREATE" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p' | head -1)"
                if [ -z "$VOL_ID" ]; then
                    fail "volume create failed: $CREATE"
                else
                    ok "volume created: $VOL_ID"
                    ATTACH="$(api -X POST "http://127.0.0.1:$MGMT/v1/volumes/$VOL_ID/attach" \
                        -H 'Content-Type: application/json' \
                        -d "{\"node\":\"$NODE\",\"mode\":\"read_write\"}")"
                    echo "  attach → $ATTACH"
                    NSID="$(echo "$ATTACH" | sed -n 's/.*"nsid":\([0-9]*\).*/\1/p' | head -1)"
                    if [ -z "$NSID" ]; then
                        fail "attach did not report an nsid (contract regression)"
                    else
                        ok "attach reported nsid=$NSID"
                    fi

                    # THE assertion: it must appear with no reconnect.
                    APPEARED=0
                    for _ in $(seq 1 20); do
                        AFTER="$(list_nsids "$CTRL")"
                        if [ "$AFTER" != "$BEFORE" ]; then APPEARED=1; break; fi
                        sleep 0.5
                    done
                    AFTER="$(list_nsids "$CTRL")"
                    info "namespaces after hot-add:  [$AFTER]"

                    if [ "$APPEARED" = "1" ]; then
                        ok "HOT-ADD VERIFIED — namespace appeared with no reconnect"
                    else
                        # Distinguish "AEN never delivered" from "never added".
                        info "no change yet; forcing a rescan to isolate the cause..."
                        nvme ns-rescan "/dev/$CTRL" >/dev/null 2>&1 || true
                        sleep 2
                        RESCAN="$(list_nsids "$CTRL")"
                        if [ "$RESCAN" != "$BEFORE" ]; then
                            fail "namespace only appeared after a manual rescan — AEN not delivered"
                            info "namespaces after rescan: [$RESCAN]"
                        else
                            fail "namespace never appeared, even after rescan — not added at all"
                        fi
                    fi

                    # ── Detach must withdraw it, again with no reconnect ──
                    api -X POST "http://127.0.0.1:$MGMT/v1/volumes/$VOL_ID/detach" \
                        -H 'Content-Type: application/json' \
                        -d "{\"node\":\"$NODE\"}" >/dev/null 2>&1
                    GONE=0
                    for _ in $(seq 1 20); do
                        NOW="$(list_nsids "$CTRL")"
                        if [ "$NOW" = "$BEFORE" ]; then GONE=1; break; fi
                        sleep 0.5
                    done
                    if [ "$GONE" = "1" ]; then
                        ok "detach withdrew the namespace"
                    else
                        info "namespaces after detach: [$(list_nsids "$CTRL")]"
                        fail "namespace still present after detach"
                    fi
                fi
            fi
        fi

        echo ""
        echo "--- target log (tail) ---"
        tail -30 "$WORKDIR/target.log" 2>/dev/null || true
    fi
fi

# ── Summary ─────────────────────────────────────────────────────────────────

if [ -n "$CUR_PHASE" ]; then
    if [ "$CUR_PHASE_FAILS" -eq 0 ]; then
        PHASE_RESULTS+=("PASS: $CUR_PHASE")
    else
        PHASE_RESULTS+=("FAIL: $CUR_PHASE ($CUR_PHASE_FAILS failures)")
    fi
fi

echo ""
echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
echo -e "${BOLD}║                      Final Summary                          ║${RESET}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
echo ""
for r in "${PHASE_RESULTS[@]}"; do
    if [[ "$r" == PASS:* ]]; then echo -e "  ${GREEN}${r}${RESET}"; else echo -e "  ${RED}${r}${RESET}"; fi
done
echo ""
if [ "$SKIPPED" -gt 0 ]; then
    echo -e "  ${YELLOW}${SKIPPED} check(s) skipped — see above for what was NOT verified${RESET}"
fi
if [ "$TOTAL_FAILURES" -eq 0 ]; then
    echo -e "  ${GREEN}${BOLD}All executed phases passed${RESET}"
    exit 0
else
    echo -e "  ${RED}${BOLD}$TOTAL_FAILURES failure(s)${RESET}"
    exit 1
fi
