#!/usr/bin/env bash
# Prepended to every transmitted remote command.
set -euo pipefail
umask 077
release_lock() {
  local directory="$HOME/.local/state/ferry-deploy"
  mkdir -p "$directory"
  if command -v flock >/dev/null 2>&1; then
    exec 9> "$directory/$NAMESPACE-$RELEASE.lock"
    flock 9
  fi
}
k() { kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" "$@"; }
h() { helm --kubeconfig "$KUBECONFIG_PATH" "$@"; }
