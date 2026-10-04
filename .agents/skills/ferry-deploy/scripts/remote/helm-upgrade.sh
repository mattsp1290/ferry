#!/usr/bin/env bash
set -euo pipefail
umask 077
lockdir="$HOME/.local/state/ferry-deploy"
mkdir -p "$lockdir"
if command -v flock >/dev/null; then exec 9> "$lockdir/$NAMESPACE-$RELEASE.lock"; flock 9; fi
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-deploy.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM
tar -xf - -C "$tmp"
set -- upgrade --install "$RELEASE" "$tmp/charts/ferry" -n "$NAMESPACE"
if [ -f "$tmp/preflight.yaml" ]; then set -- "$@" -f "$tmp/preflight.yaml"; fi
set -- "$@" -f "$tmp/values.yaml" -f "$tmp/image.yaml"
if [ "$DRY_RUN" = 1 ]; then
  if ! helm --kubeconfig "$KUBECONFIG_PATH" "$@" --dry-run=server > "$tmp/rendered" 2> "$tmp/error"; then cat "$tmp/error" >&2; exit 1; fi
  # Never emit Helm's rendered values or Secret content.
  awk '/^kind: / {kind=$2} /^metadata:/ {metadata=1; next} metadata && /^  name: / {print kind, $2; metadata=0} /^[^ ]/ {metadata=0}' "$tmp/rendered"
else
  if ! helm --kubeconfig "$KUBECONFIG_PATH" "$@" --wait --timeout "$HELM_TIMEOUT" --history-max 10 > "$tmp/output" 2> "$tmp/error"; then
    cat "$tmp/error" >&2
    helm --kubeconfig "$KUBECONFIG_PATH" status "$RELEASE" -n "$NAMESPACE" >&2 || true
    kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" get events --field-selector involvedObject.kind=Pod >&2 || true
    kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" logs -l "app.kubernetes.io/instance=$RELEASE" --tail=50 >&2 || true
    exit 1
  fi
  helm --kubeconfig "$KUBECONFIG_PATH" status "$RELEASE" -n "$NAMESPACE" -o json
fi
