#!/usr/bin/env bash
set -euo pipefail
command -v rg >/dev/null || { printf 'test requires rg\n' >&2; exit 1; }
skill=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export HOME="$tmp/home" FERRY_DEPLOY_CONFIG="$tmp/config"
mkdir -p "$HOME"
bash "$skill/scripts/deploy.sh" init >/dev/null
[ ! -e "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token" ]
printf 'test-token\n' > "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"
chmod 600 "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"
load() { bash -c 'source "$1/scripts/lib/config.sh"' _ "$skill"; }
fail() {
    local rule=$1 status=0
    load > "$tmp/out" 2>&1 || status=$?
    [ "$status" = 2 ] || { cat "$tmp/out"; return 1; }
    rg -q "$rule" "$tmp/out"
}
load
cp "$FERRY_DEPLOY_CONFIG/deploy.env" "$tmp/env"
for value in '-oProxyCommand=x' 'a@b;touch/evil' 'a b' 'a"b'; do
    sed "s|^CONTROL_SSH=.*|CONTROL_SSH=$value|" "$tmp/env" > "$FERRY_DEPLOY_CONFIG/deploy.env"
    fail CONTROL_SSH
    if rg -F -q -- "$value" "$tmp/out"; then printf 'unexpected match\n' >&2; exit 1; fi
done
cp "$tmp/env" "$FERRY_DEPLOY_CONFIG/deploy.env"
sed 's/^REGISTRY_PUSH=.*/REGISTRY_PUSH=registry/' "$tmp/env" > "$FERRY_DEPLOY_CONFIG/deploy.env"
fail REGISTRY_PUSH
cp "$tmp/env" "$FERRY_DEPLOY_CONFIG/deploy.env"
sed 's/^NAMESPACE=.*/NAMESPACE=$(id)/' "$tmp/env" > "$FERRY_DEPLOY_CONFIG/deploy.env"
fail NAMESPACE
sed '/^RELEASE=/d' "$tmp/env" > "$FERRY_DEPLOY_CONFIG/deploy.env"
fail 'missing required key RELEASE'
cp "$tmp/env" "$FERRY_DEPLOY_CONFIG/deploy.env"
printf 'NODE_SELECTOR=kubernetes.io/arch=amd64\n' >> "$FERRY_DEPLOY_CONFIG/deploy.env"
fail 'unknown key NODE_SELECTOR'
cp "$tmp/env" "$FERRY_DEPLOY_CONFIG/deploy.env"
printf 'UNKNOWN=x\n' >> "$FERRY_DEPLOY_CONFIG/deploy.env"
fail 'unknown key UNKNOWN'
cp "$tmp/env" "$FERRY_DEPLOY_CONFIG/deploy.env"
touch "$FERRY_DEPLOY_CONFIG/.DS_Store"; chmod 600 "$FERRY_DEPLOY_CONFIG/.DS_Store"
fail 'unexpected entry'; rm "$FERRY_DEPLOY_CONFIG/.DS_Store"
chmod 755 "$FERRY_DEPLOY_CONFIG/state"; fail '0700'; chmod 700 "$FERRY_DEPLOY_CONFIG/state"
# A caller cannot bypass validation by exporting the formerly proposed skip flag.
chmod 644 "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"; export FERRY_DEPLOY_SKIP_CONFIG=1; fail '0600'; unset FERRY_DEPLOY_SKIP_CONFIG; chmod 600 "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"
mv "$FERRY_DEPLOY_CONFIG/values.yaml" "$tmp/values"
ln -s "$tmp/values" "$FERRY_DEPLOY_CONFIG/values.yaml"; fail 'symlink'; rm "$FERRY_DEPLOY_CONFIG/values.yaml"
mv "$tmp/values" "$FERRY_DEPLOY_CONFIG/values.yaml"
: > "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"; fail 'non-empty'
printf 'test-token\n' > "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"
git -C "$FERRY_DEPLOY_CONFIG" init -q
fail 'git work tree|unexpected entry'
rm -rf "$FERRY_DEPLOY_CONFIG/.git"
source "$skill/scripts/lib/config.sh"
# Optional inherited settings must never bypass deploy.env validation.
HELM_TIMEOUT='unsafe;execute' IMAGE_PUSH_PATH='unsafe;execute' BASE_PUSH_PATH='unsafe;execute' bash -c '
    source "$1/scripts/lib/config.sh"
    [ "$HELM_TIMEOUT" = 5m ]
    [ "$IMAGE_PUSH_PATH" = ferry/ferry ]
    [ "$BASE_PUSH_PATH" = ferry/base ]
' _ "$skill"
source "$skill/scripts/lib/values.sh"
printf 'cache: {size: 30Gi}\n' > "$CFG/values.yaml"
flow=$(merged_values)
printf 'cache:\n  size: 30Gi\n' > "$CFG/values.yaml"
block=$(merged_values)
[ "$flow" = "$block" ]
printf 'cache: {size: 40Gi}\n' > "$CFG/state/preflight.yaml"
[ "$(merged_values | jq -r '.cache.size')" = 30Gi ]
printf 'ok: config validation, safe patterns and Helm values merging\n'

config_pattern FORGEJO_CHECK_URL https://git.example.internal
if config_pattern FORGEJO_CHECK_URL http://git.example.internal; then exit 1; fi
if config_pattern FORGEJO_CHECK_URL https://user@git.example.internal; then exit 1; fi
configure_url_policy '{"allowInsecureUrls":true}'
[ "$FERRY_ALLOW_INSECURE_URLS" = 1 ]
configure_url_policy '{}'
[ -z "${FERRY_ALLOW_INSECURE_URLS:-}" ]
