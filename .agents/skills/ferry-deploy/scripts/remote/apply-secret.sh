#!/usr/bin/env bash
set -euo pipefail
umask 077
before=$(kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" get secret "$SECRET_NAME" --ignore-not-found -o 'jsonpath={.metadata.resourceVersion}')
kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" apply --server-side --field-manager=ferry-deploy -f - >/dev/null
after=$(kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" get secret "$SECRET_NAME" -o 'jsonpath={.metadata.resourceVersion}')
if [ -z "$before" ]; then printf 'created\n'; elif [ "$before" = "$after" ]; then printf 'unchanged\n'; else printf 'changed\n'; fi
