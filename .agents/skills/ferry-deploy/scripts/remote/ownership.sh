#!/usr/bin/env bash
set -euo pipefail
umask 077
helm --kubeconfig "$KUBECONFIG_PATH" list -A --all --filter "^${RELEASE}\$" -o json
