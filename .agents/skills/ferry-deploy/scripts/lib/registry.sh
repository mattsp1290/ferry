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
read_digest() {
  local digest
  digest=$(crane_cmd "$REGISTRY_INSECURE" digest "$1") || return 1
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
