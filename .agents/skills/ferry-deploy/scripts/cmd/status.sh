#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")/../lib" && pwd)/config.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/state.sh"
report=$(remote_run status)
printf '%s' "$report" | jq -r -f "$SKILL_DIR/scripts/jq/status.jq"
state=$(printf '%s' "$report" | jq -r '.status.info.status')
warn_restart_pending
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
