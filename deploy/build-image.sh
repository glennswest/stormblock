#!/usr/bin/env bash
# The `stormblock` golden — the node's storage engine image — built here, from
# this checkout, and nowhere else (#755, stormcentral#1836: "do repos, repos
# are not expensive"; stormcos's stage mode is retired).
#
#   deploy/build-image.sh stormblock OUT
#
# stormcentral's component build runs it for the `stormblock` component (kind
# `tree`: `deploy/build-golden.sh stormblock OUT`, which execs this) in a
# checkout fetched at the exact commit, on a fresh build VM, as root of the
# build's own user namespace. OUT is copied into the sealed read-only golden
# `golden-stormblock-<build>`, sized from the content, with `SBOM/crates.csv`
# added by the platform. It is the `stormblock` member of the system pallet
# (stormcos deploy/image.toml), laid at /pallets/stormblock and run by
# stormpump's 00-stormblock boot unit as
# `/usr/bin/stormblock adopt-ublk --api 0.0.0.0:9090 --data-dir /run/stormblock/engine`.
#
# The tree, as stormcos's deploy/build-goldens.sh staged it until 2026-11-26
# (its "stormblock — the storage everything clones from" recipe, moved here
# with its content unchanged):
#
#   usr/bin/stormblock      the engine, static x86_64 musl, the `dist`
#                           profile (fat LTO, one codegen unit, #209) — the
#                           rule the stage and stormcentral's component builds
#                           share: `dist` when Cargo.toml defines one
#   etc/stormblock/stormblock.toml   the engine's config (below)
#   etc/stormcert/          empty: the unit binds tier-0's stormcert pair, CA
#                           and forge trust there, read-only (#296)
#   srv/registry/           empty: the node registry's data, read-only, for
#                           media and blob imports (stormcos#373)
#   data/  run/stormblock/  empty: the unit's data volume and the host's
#                           /run/stormblock tmpfs
#   dev/ proc/ sys/ tmp/    empty mount points
#
# stormpump mounts only onto a directory the image has (stormpump#102), so
# every one of these is a mount point the unit relies on.
#
# The initramfs (scripts/build-stormblock-initramfs.sh) is not this golden:
# the platform builds it from the modules golden, as before.
#
# x86_64 only, as in stage mode: any other GOLDEN_ARCH is refused.
set -euo pipefail
name=${1:?usage: deploy/build-image.sh stormblock OUT}
out=${2:?usage: deploy/build-image.sh stormblock OUT}
[ "$name" = stormblock ] || { echo "build-image: this repo builds only 'stormblock', not '$name'" >&2; exit 2; }
arch=${GOLDEN_ARCH:-x86_64}
[ "$arch" = x86_64 ] || { echo "build-image: stormblock is built for x86_64 only (GOLDEN_ARCH=$arch)" >&2; exit 2; }
cd "$(dirname "$0")/.."
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true
TARGET=x86_64-unknown-linux-musl
command -v rustup >/dev/null 2>&1 && rustup target add "$TARGET" >/dev/null 2>&1 || true
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
commit=$(git rev-parse HEAD)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
profile=release; grep -q '^\[profile\.dist\]' Cargo.toml && profile=dist

# The root package only, default features (iscsi, stormfs-data), as the stage
# built it: `cargo build` at the root of a workspace whose root is a package.
echo "==> cargo build --profile $profile --locked --target $TARGET -p stormblock ($commit)"
cargo build --profile "$profile" --locked --target "$TARGET" --quiet -p stormblock
BIN="$CARGO_TARGET_DIR/$TARGET/$profile/stormblock"
[ -x "$BIN" ] || { echo "build-image: cargo produced no $BIN" >&2; exit 1; }
# Static: the golden has no loader and no libc.
if command -v file >/dev/null 2>&1; then
    file "$BIN" | grep -Eq 'static-pie linked|statically linked' || { echo "build-image: not static: $(file "$BIN")" >&2; exit 1; }
fi
# The binary is this crate at this version.
got=$("$BIN" --version)
[ "$got" = "stormblock $version" ] || { echo "build-image: the binary says '$got', Cargo.toml says $version" >&2; exit 1; }

dirs="usr usr/bin etc etc/stormblock etc/stormcert srv srv/registry data dev proc sys run run/stormblock tmp"
for d in $dirs; do mkdir -p "$out/$d"; done
chmod 0755 "$out"
for d in $dirs; do chmod 0755 "$out/$d"; done
install -m 0755 "$BIN" "$out/usr/bin/stormblock"

# A config, rather than the defaults.
#
# No [[drives]]: this engine is handed the slab by `adopt-ublk`, which opens
# the disk itself once the previous server has stood down. One slab holds
# every volume on this node — the goldens, their clones, and the data
# containers — so there is nothing a second disk would add.
#
# /data, not /var/lib/anything. Every container that keeps state gets one data
# container mounted at the same path, so nothing has to be special-cased.
#
# TLS on :9090 (stormcos#374, #203). The pair and CA are stormcert's, in
# tier-0, which the unit binds read-only at /etc/stormcert. The engine starts
# long before they exist: until then it serves plain, then HTTPS with no
# restart (#483). `optional` takes plain and TLS on one port while callers move
# one golden at a time (#525); stormblock_mgmt_plain_requests_total says who is
# left, and at none past boot this becomes `required` and the key goes. The
# boot port is plain for good: pre-boot firmware has no TLS (#524, the owner's
# option A), and it serves the boot verbs alone.
#
# Forge's CA and CRL (stormcos#497, #379/#453): stormcert-forge-trust keeps
# them in tier-0, and the engine takes each up when it appears (#483). Only CN
# stormcentral from forge's CA is admin here; with either file absent no forge
# certificate proves anything, which is every node today: forge's CRL reaches
# a node only with stormcert#124.
cat > "$out/etc/stormblock/stormblock.toml" << 'CONF'
[management]
listen_addr = "0.0.0.0:9090"
data_dir = "/data"
tls_cert = "/etc/stormcert/stormblock.crt"
tls_key = "/etc/stormcert/stormblock.key"
tls_client_ca = "/etc/stormcert/ca.crt"
tls_mode = "optional"
boot_listen = "0.0.0.0:9089"
tls_admin_ca = "/etc/stormcert/forge-ca.crt"
tls_admin_crl = "/etc/stormcert/forge.crl"
tls_admin_subjects = ["stormcentral"]

# An optional kernel image from forge (#571, stormblock#383). The node's enrolled
# pair is forge-enroll's, on kept data (#486). Forge's engine serves its node CA's
# certificate, which is what apiserver-ca.crt holds, not the forge CA (stormcert#136).
# All absent until the node has enrolled, and the engine answers 409 until then.
[kernel]
client_cert = "/run/stormblock/system-data/identity/forge-node.crt.pem"
client_key = "/run/stormblock/system-data/identity/forge-node.key.pem"
forge_ca = "/run/stormblock/forge/apiserver-ca.crt"
CONF
chmod 0644 "$out/etc/stormblock/stormblock.toml"
# That the engine takes every key of it is a unit test of the config's
# (`mgmt::config::tests::the_golden_config_is_one_the_engine_reads`, which
# reads it out of this file), so a renamed field fails the suite, not a node.

# Read back what was copied: a short copy is never sealed (stormcentral#716).
[ "$(sha256sum < "$out/usr/bin/stormblock")" = "$(sha256sum < "$BIN")" ] || { echo "build-image: usr/bin/stormblock does not read back as built" >&2; exit 1; }

echo "==> stormblock golden tree in $out: $got at $commit"
(cd "$out" && find . -mindepth 1 -printf '%M %8s %P\n' | LC_ALL=C sort -k3)
sha256sum "$out/usr/bin/stormblock" "$out/etc/stormblock/stormblock.toml" | sed "s|$out/||"
