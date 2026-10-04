#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../../../.." && pwd)
umask 077
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-status-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/cfg/state"
export FERRY_DEPLOY_CONFIG="$tmp/cfg" CONTROL_SSH=control NAMESPACE=ferry RELEASE=ferry KUBECONFIG_PATH=/config HELM_TIMEOUT=5m IMAGE_PULL_REPOSITORY=localhost:5000/ferry/ferry
bash "$root/.agents/skills/ferry-deploy/scripts/deploy.sh" init >/dev/null
printf 'obviously-fake-token\n' > "$tmp/cfg/secrets/forgejo-token"
chmod 600 "$tmp/cfg/secrets/forgejo-token"
export PATH="$tmp/bin:$PATH"
cat > "$tmp/bin/ssh" <<'SH'
#!/usr/bin/env bash
printf '%s\n' '{"status":{"info":{"status":"pending-upgrade"},"version":3},"values":{"image":{"digest":"sha256:abc","version":"123456789012"}},"pods":{"items":[]},"history":[{"revision":1,"status":"superseded"},{"revision":2,"status":"deployed"},{"revision":3,"status":"pending-upgrade"}]}'
SH
chmod +x "$tmp/bin/ssh"
printf '{"action":"rollout"}\n' > "$tmp/cfg/state/deployments.jsonl"
: > "$tmp/cfg/state/secret-restart-pending"
bash "$root/.agents/skills/ferry-deploy/scripts/cmd/status.sh" > "$tmp/output" 2>&1
grep -q 'rollback ferry 2' "$tmp/output"
grep -q 'image.digest: sha256:abc' "$tmp/output"
grep -q 'local log:' "$tmp/output"
grep -q 'credentials changed and the pod has not reloaded them: run deploy restart' "$tmp/output"
