#!/usr/bin/env bash
release_lock
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-deploy.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM
tar -xf - -C "$tmp"
set -- upgrade --install "$RELEASE" "$tmp/charts/ferry" -n "$NAMESPACE"
if [ -f "$tmp/preflight.yaml" ]; then set -- "$@" -f "$tmp/preflight.yaml"; fi
set -- "$@" -f "$tmp/values.yaml" -f "$tmp/image.yaml"
if [ "$DRY_RUN" = 1 ]; then
  if ! h "$@" --dry-run=server > "$tmp/rendered" 2> "$tmp/error"; then cat "$tmp/error" >&2; exit 1; fi
  # Never emit Helm's rendered values or Secret content.
  awk '/^kind: / {kind=$2} /^metadata:/ {metadata=1; next} metadata && /^  name: / {print kind, $2; metadata=0} /^[^ ]/ {metadata=0}' "$tmp/rendered"
else
  if ! h "$@" --wait --timeout "$HELM_TIMEOUT" --history-max 10 > "$tmp/output" 2> "$tmp/error"; then
    cat "$tmp/error" >&2
    h status "$RELEASE" -n "$NAMESPACE" >&2 || true
    k get events --field-selector involvedObject.kind=Pod >&2 || true
    k logs -l "app.kubernetes.io/instance=$RELEASE" --tail=50 >&2 || true
    exit 1
  fi
  h status "$RELEASE" -n "$NAMESPACE" -o json
fi
