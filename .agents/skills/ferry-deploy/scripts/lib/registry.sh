#!/usr/bin/env bash
# Config-free registry helpers shared with CI.
crane_cmd() {
  local insecure=$1
  shift
  case $insecure in
    true) crane --insecure "$@" ;;
    false) crane "$@" ;;
    *) printf 'registry: invalid insecure setting\n' >&2; return 2 ;;
  esac
}
# Status 3 means the registry explicitly reported that the manifest is absent.
# Any other error (auth, transport, invalid digest) stops publishing.
read_digest() {
  local digest error status=0
  error=$(mktemp "${TMPDIR:-/tmp}/ferry-registry.XXXXXX") || return 1
  digest=$(crane_cmd "$REGISTRY_INSECURE" digest "$1" 2> "$error") || status=$?
  if [ "$status" != 0 ]; then
    if grep -Eq '(^|[^A-Z_])(MANIFEST_UNKNOWN|NAME_UNKNOWN)([^A-Z_]|$)' "$error"; then
      rm -f "$error"; return 3
    fi
    printf 'registry: digest lookup failed; check connectivity and authentication\n' >&2
    rm -f "$error"; return 1
  fi
  rm -f "$error"
  [[ $digest =~ ^sha256:[a-f0-9]{64}$ ]] || { printf 'registry: invalid manifest digest\n' >&2; return 1; }
  printf '%s\n' "$digest"
}
base_tag() {
  if command -v sha256sum >/dev/null; then
    sha256sum docker/base.Dockerfile | cut -c1-12
  else
    shasum -a 256 docker/base.Dockerfile | cut -c1-12
  fi
}
