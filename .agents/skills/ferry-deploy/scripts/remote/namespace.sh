#!/usr/bin/env bash
set -euo pipefail
umask 077
kubectl --kubeconfig "$KUBECONFIG_PATH" get namespace "$NAMESPACE" --ignore-not-found -o json
