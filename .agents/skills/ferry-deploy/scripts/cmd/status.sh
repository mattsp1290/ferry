#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")/../lib" && pwd)/config.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
report=$(remote_run status)
printf '%s' "$report" | jq -r '"release: \(.status.info.status) revision: \(.status.version)", "image.digest: \(.values.image.digest)", "image.version: \(.values.image.version)", (.pods.items[] | "pod: \(.metadata.name) uid: \(.metadata.uid) phase: \(.status.phase) readiness: \([.status.conditions[]? | select(.type == "Ready") | .status][0] // "Unknown") restarts: \([.status.containerStatuses[]?.restartCount] | add // 0) node: \(.spec.nodeName)")'
state=$(printf '%s' "$report" | jq -r '.status.info.status')
if [ -f "$CFG/state/secret-restart-pending" ]; then
    printf 'credentials: changed since the pod started; run deploy restart\n'
fi
case "$state" in
    pending-*|failed)
        revision=$(printf '%s' "$report" | jq -r '[.history[] | select(.status == "deployed" or .status == "superseded")] | sort_by(.revision | tonumber) | last | .revision // empty')
        if [ -n "$revision" ]; then
            printf 'recovery on control host: helm --kubeconfig %s rollback %s %s -n %s --wait --timeout %s\n' "$KUBECONFIG_PATH" "$RELEASE" "$revision" "$NAMESPACE" "$HELM_TIMEOUT"
        else
            printf 'first-install recovery: see references/rollout.md before Helm uninstall; no revision is available\n'
        fi ;;
esac
if [ -f "$CFG/state/deployments.jsonl" ]; then printf 'local log: %s\n' "$(tail -n 1 "$CFG/state/deployments.jsonl")"; fi
