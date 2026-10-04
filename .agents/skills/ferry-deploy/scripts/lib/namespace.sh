#!/usr/bin/env bash
# Read-only ownership guard shared by preflight and rollout. Absence is a
# distinct outcome so only preflight can create the namespace.
namespace_guard() {
    local releases namespace
    releases=$(remote_run ownership) || return 1
    printf '%s' "$releases" | jq -e 'type == "array"' >/dev/null || return 1
    if printf '%s' "$releases" | jq -e --arg r "$RELEASE" --arg n "$NAMESPACE" 'any(.[]; .name==$r and .namespace!=$n)' >/dev/null; then
        printf 'release exists outside target namespace\n' >&2
        return 1
    fi
    namespace=$(remote_run namespace) || return 1
    [ -n "$namespace" ] || return 3
    printf '%s' "$namespace" | jq -e 'type == "object"' >/dev/null || return 1
    if ! printf '%s' "$namespace" | jq -e '.metadata.labels["app.kubernetes.io/managed-by"]=="ferry-deploy"' >/dev/null && ! printf '%s' "$releases" | jq -e --arg r "$RELEASE" --arg n "$NAMESPACE" 'any(.[]; .name==$r and .namespace==$n)' >/dev/null; then
        printf 'namespace is not owned by ferry-deploy or the release\n' >&2
        return 1
    fi
}
# Bind probe results to the actual values inputs, including chart defaults.
preflight_values_fingerprint() {
    python3 - "$REPO_ROOT/charts/ferry/values.yaml" "$CFG/values.yaml" <<'PY'
import hashlib, pathlib, sys
fingerprint = hashlib.sha256()
for name in sys.argv[1:]:
    content = pathlib.Path(name).read_bytes()
    fingerprint.update(len(content).to_bytes(8, 'big'))
    fingerprint.update(content)
print(fingerprint.hexdigest())
PY
}
