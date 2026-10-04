#!/usr/bin/env bash
set -euo pipefail
umask 077
status=$(helm --kubeconfig "$KUBECONFIG_PATH" status "$RELEASE" -n "$NAMESPACE" -o json)
values=$(helm --kubeconfig "$KUBECONFIG_PATH" get values "$RELEASE" -n "$NAMESPACE" -a -o json)
pods=$(kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" get pods -l "app.kubernetes.io/instance=$RELEASE" -o json)
history=$(helm --kubeconfig "$KUBECONFIG_PATH" history "$RELEASE" -n "$NAMESPACE" -o json)
printf '{"status":%s,"values":%s,"pods":%s,"history":%s}\n' "$status" "$values" "$pods" "$history"
