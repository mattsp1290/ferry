#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/../lib/config.sh"
source "$SKILL_DIR/scripts/lib/values.sh"
source "$REPO_ROOT/scripts/lib/extract-configmap.sh"
forgejo=0; github=0; show=0
for argument in "$@"; do
    case "$argument" in --forgejo) forgejo=1 ;; --github) github=1 ;; --show-values) show=1 ;; *) config_error "Unknown check option" ;; esac
done
for tool in helm crane docker jq cargo ssh curl python3; do command -v "$tool" >/dev/null || { printf 'Missing tool: %s\n' "$tool" >&2; exit 1; }; done
helm version >/dev/null
crane version >/dev/null
docker info >/dev/null
jq --version >/dev/null
cargo zigbuild --help >/dev/null
remote_version=$(ssh -o BatchMode=yes -o ConnectTimeout=15 "$CONTROL_SSH" "helm --kubeconfig $KUBECONFIG_PATH version --short")
case "$remote_version" in v3.*) ;; *) printf 'Control-host Helm major version must be 3\n' >&2; exit 1 ;; esac
ssh -o BatchMode=yes -o ConnectTimeout=15 "$CONTROL_SSH" "kubectl --kubeconfig $KUBECONFIG_PATH get nodes" >/dev/null
owner_values_json | jq -e '.image // {} | has("repository") or has("digest") or has("version")' >/dev/null && config_error "$CFG/values.yaml: image.repository, image.digest and image.version are supplied by the skill"
values=$(merged_values)
configure_url_policy "$values"
printf '%s\n' "$values" | jq -e '(.config.health.listen // "0.0.0.0:8080") | test(":8080$")' >/dev/null || config_error "config.health.listen must use port 8080 for the chart probes"
if [ "$show" = 1 ]; then printf '%s\n' "$values"; fi
binary=$(ferry_binary)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-check.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
set -- -f "$CFG/values.yaml"
if [ -f "$CFG/state/preflight.yaml" ]; then set -- -f "$CFG/state/preflight.yaml" "$@"; fi
helm template "$RELEASE" "$REPO_ROOT/charts/ferry" "$@" --set image.repository=placeholder --set image.digest=sha256:0 --show-only templates/configmap.yaml > "$tmp/configmap.yaml"
extract_toml "$tmp/configmap.yaml" > "$tmp/ferry.toml"
"$binary" check-config --config "$tmp/ferry.toml"
# Generate curl configuration from an inherited descriptor; the shell never holds a token.
check_api() {
    python3 "$SKILL_DIR/scripts/lib/token-curl.py" "$1" "$2" "$3"
}
if [ "$forgejo" = 1 ]; then
    url=${FORGEJO_CHECK_URL:-$(printf '%s\n' "$values" | jq -er '.config.forgejo.url')}
    username=$(printf '%s\n' "$values" | jq -er '.config.forgejo.username')
    with_token_fd "$CFG/secrets/forgejo-token" 3 check_api "$url/api/v1/user" forgejo "$username"
fi
if [ "$github" = 1 ]; then
    [ -s "$CFG/secrets/github-token" ] || config_error "$CFG/secrets/github-token: required for --github"
    with_token_fd "$CFG/secrets/github-token" 3 check_api https://api.github.com/user github ''
fi
printf 'ok: config, tools and control host\n'
