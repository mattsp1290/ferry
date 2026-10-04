#!/usr/bin/env bash
set -euo pipefail
umask 077
printf '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"%s","labels":{"app.kubernetes.io/managed-by":"ferry-deploy"}}}\n' "$NAMESPACE" | kubectl --kubeconfig "$KUBECONFIG_PATH" apply -f - >/dev/null
