#!/usr/bin/env bash
set -euo pipefail
umask 077
mkdir -p "$HOME/.local/state/ferry-deploy"
if command -v flock >/dev/null 2>&1; then
    exec 8>"$HOME/.local/state/ferry-deploy/$NAMESPACE-$RELEASE.lock"
    flock 8
fi
# Helm's table uses revision as its first column. Find the status word before
# chart/version/description fields; updated timestamps have variable spacing.
history=$(helm --kubeconfig "$KUBECONFIG_PATH" history "$RELEASE" -n "$NAMESPACE")
revision=$(printf '%s\n' "$history" | awk -v wanted="${REVISION:-}" '
    $1 ~ /^[0-9]+$/ {
        status=""
        for (i=2;i<=NF;i++) {
            if ($i == "deployed" || $i == "superseded" || $i == "failed" || $i ~ /^pending-/ || $i == "uninstalled" || $i == "uninstalling" || $i == "unknown") { status=$i; break }
        }
        if (wanted != "" && $1 == wanted && (status == "superseded" || status == "deployed")) selected=$1
        if (wanted == "" && status == "superseded" && $1+0 > selected+0) selected=$1
    }
    END { if (selected != "") print selected }
')
if [ -z "$revision" ]; then
    printf 'No successful revision available for rollback. To stop ferry on the control host:\n' >&2
    printf 'kubectl --kubeconfig %s -n %s scale deployment -l app.kubernetes.io/instance=%s --replicas=0\n' "$KUBECONFIG_PATH" "$NAMESPACE" "$RELEASE" >&2
    exit 1
fi
helm --kubeconfig "$KUBECONFIG_PATH" rollback "$RELEASE" "$revision" -n "$NAMESPACE" --wait --timeout "$HELM_TIMEOUT" >&2
status=$(helm --kubeconfig "$KUBECONFIG_PATH" status "$RELEASE" -n "$NAMESPACE" -o json)
values=$(helm --kubeconfig "$KUBECONFIG_PATH" get values "$RELEASE" -n "$NAMESPACE" -a -o json)
printf '{"status":%s,"values":%s}\n' "$status" "$values"
