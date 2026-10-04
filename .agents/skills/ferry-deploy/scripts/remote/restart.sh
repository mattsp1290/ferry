#!/usr/bin/env bash
set -euo pipefail
umask 077
mkdir -p "$HOME/.local/state/ferry-deploy"
if command -v flock >/dev/null 2>&1; then
    exec 8>"$HOME/.local/state/ferry-deploy/$NAMESPACE-$RELEASE.lock"
    flock 8
fi
names=$(kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" get deployment -l "app.kubernetes.io/instance=$RELEASE" -o 'jsonpath={range .items[*]}{.metadata.name}{"\n"}{end}')
[ "$(printf '%s\n' "$names" | awk 'NF {count++} END {print count+0}')" = 1 ] || { printf 'expected exactly one release Deployment\n' >&2; exit 1; }
name=$names
kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" rollout restart "deployment/$name"
kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" rollout status "deployment/$name" --timeout="$HELM_TIMEOUT"
