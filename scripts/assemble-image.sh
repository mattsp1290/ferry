#!/usr/bin/env bash
# Daemonless assembly of the ferry runtime image with crane.
#
#   assemble-image.sh <base-ref@sha256:...> <target-repository> <tag> [extra-tag...]
#
# Appends one layer holding /usr/local/bin/ferry to the digest-pinned base,
# sets entrypoint, cmd, and user (the same values as docker/Dockerfile), pushes
# <target-repository>:<tag> and every extra tag, and prints the pushed manifest
# digest as the last line on stdout. Everything else goes to stderr.
#
# Environment:
#   FERRY_BINARY             binary to package (default dist/linux-amd64/ferry)
#   FERRY_INSECURE_REGISTRY  registry host[:port] that may be reached without
#                            TLS; crane gets --insecure only for commands that
#                            touch a reference on that host
set -euo pipefail

cd "$(dirname "$0")/.."

# Keep in step with docker/Dockerfile.
RUN_USER=10001

die() { echo "assemble-image: $*" >&2; exit 1; }

if [ "$#" -lt 3 ]; then
  die "usage: $0 <base-ref@sha256:...> <target-repository> <tag> [extra-tag...]"
fi
base=$1
repository=$2
shift 2
tags=("$@")
binary=${FERRY_BINARY:-dist/linux-amd64/ferry}

case $base in
  *@sha256:?*) ;;
  *) die "base reference must be pinned by digest (name@sha256:...): $base" ;;
esac
[ -f "$binary" ] || die "binary not found: $binary (run scripts/build-binaries.sh)"
command -v crane >/dev/null || die "crane is required"
command -v jq >/dev/null || die "jq is required"

# Registry host of a reference: the first path segment when it looks like a
# host (contains '.' or ':', or is localhost), else Docker Hub.
registry_host() {
  local first=${1%%/*}
  case $first in
    *.* | *:* | localhost) printf '%s' "$first" ;;
    *) printf 'index.docker.io' ;;
  esac
}

# Prints --insecure when any given reference is on FERRY_INSECURE_REGISTRY.
# crane applies the flag to every reference of one command, so a command that
# mixes registries gets it when one of them needs it.
insecure_flag() {
  [ -n "${FERRY_INSECURE_REGISTRY:-}" ] || return 0
  local ref
  for ref in "$@"; do
    if [ "$(registry_host "$ref")" = "$FERRY_INSECURE_REGISTRY" ]; then
      printf '%s' "--insecure"
      return 0
    fi
  done
}

platform=(--platform linux/amd64)

config=$(crane config $(insecure_flag "$base") "${platform[@]}" "$base") \
  || die "cannot read the config of $base"
os=$(printf '%s' "$config" | jq -r '.os // ""')
arch=$(printf '%s' "$config" | jq -r '.architecture // ""')
[ "$os" = linux ] && [ "$arch" = amd64 ] \
  || die "base must be linux/amd64, got os=$os architecture=$arch"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Layer with exactly one entry, usr/local/bin/ferry, owned by 0:0, mode 0755.
# Archiving the file path alone adds no parent directory entries.
mkdir -p "$work/stage/usr/local/bin"
install -m 0755 "$binary" "$work/stage/usr/local/bin/ferry"
export COPYFILE_DISABLE=1 # keeps macOS tar from adding AppleDouble entries
if tar --version 2>&1 | grep -qi bsdtar; then
  # bsdtar has no --owner/--mode; the mode comes from the staged file.
  tar -cf "$work/layer.tar" --uid 0 --gid 0 --uname root --gname root \
    -C "$work/stage" usr/local/bin/ferry
else
  tar -cf "$work/layer.tar" --owner=0 --group=0 --numeric-owner --mode=0755 \
    -C "$work/stage" usr/local/bin/ferry
fi
entries=$(tar -tf "$work/layer.tar")
[ "$entries" = "usr/local/bin/ferry" ] || die "unexpected layer entries: $entries"

target="$repository:${tags[0]}"

# crane append pushes base + layer and prints the new digest (to stderr here);
# crane mutate then rewrites the manifest in the registry with the runtime
# settings and pushes it under the same tag.
crane append $(insecure_flag "$base" "$target") "${platform[@]}" \
  -b "$base" -f "$work/layer.tar" -t "$target" >&2
crane mutate $(insecure_flag "$target") "${platform[@]}" \
  --entrypoint /usr/bin/tini,--,/usr/local/bin/ferry \
  --cmd run \
  -u "$RUN_USER" \
  -t "$target" "$target" >&2

for tag in "${tags[@]:1}"; do
  crane tag $(insecure_flag "$target") "$target" "$tag" >&2
done

# The last stdout line: the digest of the pushed manifest.
crane digest $(insecure_flag "$target") "$target"
