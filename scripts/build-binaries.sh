#!/usr/bin/env bash
# Cross-builds the linux/amd64 ferry binary into dist/linux-amd64/ferry.
# Needs zig and cargo-zigbuild.
set -euo pipefail

cd "$(dirname "$0")/.."

# build.rs reads only the environment variable.
if [ -z "${FERRY_GIT_SHA:-}" ]; then
  FERRY_GIT_SHA="$(git rev-parse --short=12 HEAD)"
fi
export FERRY_GIT_SHA

# The .2.36 suffix is the glibc version of Debian bookworm, the base image.
# Change it together with the base image distribution.
target=x86_64-unknown-linux-gnu.2.36
cargo zigbuild --release --target "$target"

mkdir -p dist/linux-amd64
cp target/x86_64-unknown-linux-gnu/release/ferry dist/linux-amd64/ferry
echo "built dist/linux-amd64/ferry (FERRY_GIT_SHA=$FERRY_GIT_SHA)" >&2
