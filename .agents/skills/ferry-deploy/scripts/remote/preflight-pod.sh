#!/usr/bin/env bash
set -euo pipefail
umask 077
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-deploy.XXXXXX")
name=$POD_NAME
cleanup() {
  kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" delete pod,pvc -l "ferry-deploy-preflight=$name" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
cat > "$tmp/apply.json"
kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" apply -f "$tmp/apply.json" >/dev/null
end=$((SECONDS + 300))
while :; do
  reasons=$(kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" get pod "$name" -o 'jsonpath={range .status.containerStatuses[*]}{.state.waiting.reason}{"\n"}{end}{range .status.initContainerStatuses[*]}{.state.waiting.reason}{"\n"}{end}')
  case "$reasons" in *ErrImagePull*|*ImagePullBackOff*) echo 'digest is not pullable through IMAGE_PULL_REPOSITORY on the selected node' >&2; exit 1;; esac
  phase=$(kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" get pod "$name" -o 'jsonpath={.status.phase}')
  case "$phase" in Succeeded|Failed) break;; esac
  [ "$SECONDS" -lt "$end" ] || { echo 'preflight pod timed out' >&2; exit 1; }
  sleep 2
done
kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" logs "$name"
printf 'phase:%s\n' "$phase"
