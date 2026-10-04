#!/usr/bin/env bash
set -euo pipefail
umask 077
kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NAMESPACE" get pods -l "app.kubernetes.io/instance=$RELEASE" -o 'jsonpath={range .items[*]}{.metadata.uid}{"\n"}{end}' | sort
