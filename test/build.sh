#!/bin/sh
# Build stormblock's test image context for the commit checked out (#139).
#
#   test/build.sh [target]        default x86_64-unknown-linux-musl
#
# Per stormcentral docs/test-standard.md this runs first, in the checkout on
# the build box, with cargo, and stages the two static binaries in
# test/.stage/; test/Containerfile (context: the repo root) packages them
# FROM scratch. One image serves all three suites (`/test <suite>`).
# With STAGE_ONLY=1 it stops after staging test/.stage/ and prints its path;
# otherwise it also runs `podman build` and tags stormblock-test.
set -eu
target=${1:-x86_64-unknown-linux-musl}
root=$(cd "$(dirname "$0")/.." && pwd)
commit=$(git -C "$root" rev-parse HEAD)

cargo build --release --locked --target "$target" -p stormblock -p stormblock-test --manifest-path "$root/Cargo.toml"
tdir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" |
    sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
stage="$root/test/.stage"
rm -rf "$stage"
mkdir -p "$stage"
cp "$tdir/$target/release/stormblock" "$tdir/$target/release/stormblock-test" "$stage/"

if [ "${STAGE_ONLY:-0}" = 1 ]; then
    echo "$stage"
    exit 0
fi
podman build -f "$root/test/Containerfile" --ignorefile "$root/test/.containerignore" --build-arg COMMIT="$commit" -t stormblock-test "$root"
