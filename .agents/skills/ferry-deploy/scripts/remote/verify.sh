#!/usr/bin/env bash
set -euo pipefail
umask 077
status=$(helm --kubeconfig "$KUBECONFIG_PATH" status "$RELEASE" -n "$NAMESPACE" -o json)
values=$(helm --kubeconfig "$KUBECONFIG_PATH" get values "$RELEASE" -n "$NAMESPACE" -a -o json)
pods=$(kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" get pods -l "app.kubernetes.io/instance=$RELEASE" -o json)
nodes=$(kubectl --kubeconfig "$KUBECONFIG_PATH" get nodes -o json)
printf '{"status":%s,"values":%s,"pods":%s,"nodes":%s}\nFERRY_LOGS\n' "$status" "$values" "$pods" "$nodes"
pod_info=$(kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" get pods -l "app.kubernetes.io/instance=$RELEASE" -o 'jsonpath={range .items[*]}{.metadata.name}{"\t"}{.status.startTime}{"\n"}{end}')
if [ "$(printf '%s\n' "$pod_info" | awk 'NF {count++} END {print count+0}')" = 1 ]; then
    IFS="$(printf '\t')" read -r pod started <<< "$pod_info"
    if [ -n "$started" ]; then
        kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" logs "$pod" -c ferry --since=15m
    fi
fi
