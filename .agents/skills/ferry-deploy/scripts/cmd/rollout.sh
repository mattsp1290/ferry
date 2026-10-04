#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/../lib/config.sh"
source "$SKILL_DIR/scripts/lib/values.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/secret.sh"
source "$SKILL_DIR/scripts/lib/namespace.sh"
umask 077
digest= version=
while [ "$#" -gt 0 ]; do
  case "$1" in --digest|--version) [ "$#" -ge 2 ] || config_error 'missing rollout option value'; key=$1; shift; if [ "$key" = --digest ]; then digest=$1; else version=$1; fi;; *) config_error 'usage: deploy rollout [--digest sha256:... --version 12-hex]';; esac
  shift
done
cd "$REPO_ROOT"
[ -z "$(git status --porcelain)" ] || config_error 'rollout requires a clean work tree'
commit=$(git rev-parse --short=12 HEAD)
if [ -n "$digest" ] || [ -n "$version" ]; then
  [[ "$digest" =~ ^sha256:[a-f0-9]{64}$ ]] && [[ "$version" =~ ^[a-f0-9]{12}$ ]] || config_error 'digest and version must both be supplied and valid'
else
  [ -f "$CFG/state/last-publish.json" ] || config_error 'run deploy publish first'
  version=$(jq -r '.commit' "$CFG/state/last-publish.json")
  [ "$version" = "$commit" ] || config_error 'run deploy publish first: checked-out commit differs'
  digest=$(jq -r '.image_digest' "$CFG/state/last-publish.json")
  [[ "$digest" =~ ^sha256:[a-f0-9]{64}$ ]] || config_error 'invalid published digest'
fi
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-deploy.XXXXXX")
revision= completed=false
cleanup() {
  code=$?
  trap - EXIT
  rm -rf "$tmp"
  if [ -f "$CFG/state/secret-restart-pending" ]; then
    printf 'credentials changed and the pod has not reloaded them: run deploy restart\n' >&2
  fi
  if [ "$completed" != true ]; then
    jq -nc --arg time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg commit "$version" --arg digest "$digest" --arg revision "$revision" '{time:$time,commit:$commit,digest:$digest,revision:$revision,action:"rollout",result:"failed"}' >> "$CFG/state/deployments.jsonl"
  fi
  exit "$code"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
[ -f "$CFG/state/preflight.json" ] && [ -f "$CFG/state/preflight.yaml" ] || config_error 'run deploy preflight first'
fingerprint=$(preflight_values_fingerprint)
jq -e --arg namespace "$NAMESPACE" --arg release "$RELEASE" --arg digest "$digest" --arg version "$version" --arg control_ssh "$CONTROL_SSH" --arg kubeconfig_path "$KUBECONFIG_PATH" --arg image_pull_repository "$IMAGE_PULL_REPOSITORY" --arg values_sha256 "$fingerprint" '.namespace==$namespace and .release==$release and .digest==$digest and .version==$version and .control_ssh==$control_ssh and .kubeconfig_path==$kubeconfig_path and .image_pull_repository==$image_pull_repository and .values_sha256==$values_sha256' "$CFG/state/preflight.json" >/dev/null || config_error 'run deploy preflight first: target or image differs'
if namespace_guard; then :; else
  guard_result=$?
  [ "$guard_result" != 3 ] || printf 'run deploy preflight first: namespace is absent\n' >&2
  exit 1
fi
mkdir -p "$tmp/charts"
cp -R "$REPO_ROOT/charts/ferry" "$tmp/charts/ferry"
cp "$CFG/values.yaml" "$tmp/values.yaml"
[ ! -f "$CFG/state/preflight.yaml" ] || cp "$CFG/state/preflight.yaml" "$tmp/preflight.yaml"
printf 'image:\n  repository: "%s"\n  digest: "%s"\n  version: "%s"\n' "$IMAGE_PULL_REPOSITORY" "$digest" "$version" > "$tmp/image.yaml"
set -- lint "$REPO_ROOT/charts/ferry"
[ ! -f "$tmp/preflight.yaml" ] || set -- "$@" -f "$tmp/preflight.yaml"
helm "$@" -f "$tmp/values.yaml" -f "$tmp/image.yaml" >&2
set -- charts values.yaml image.yaml
[ ! -f "$tmp/preflight.yaml" ] || set -- "$@" preflight.yaml
tar -cf "$tmp/bundle.tar" -C "$tmp" "$@"
remote_run helm-upgrade DRY_RUN=1 < "$tmp/bundle.tar" >&2
apply_secret >&2
before=$(remote_run snapshot)
revision=$(remote_run helm-upgrade DRY_RUN=0 < "$tmp/bundle.tar" | jq -r '.version')
after=$(remote_run snapshot)
if [ -f "$CFG/state/secret-restart-pending" ]; then
  if [ "$before" = "$after" ]; then remote_run restart >&2; fi
  rm "$CFG/state/secret-restart-pending"
fi
jq -nc --arg time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg commit "$version" --arg digest "$digest" --arg revision "$revision" '{time:$time,commit:$commit,digest:$digest,revision:$revision,action:"rollout",result:"success"}' >> "$CFG/state/deployments.jsonl"
completed=true
printf '%s\n' "$revision"
