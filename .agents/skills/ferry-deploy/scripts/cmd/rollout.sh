#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/../lib/config.sh"
source "$SKILL_DIR/scripts/lib/values.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/secret.sh"
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
apply_secret >&2
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-deploy.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM
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
before=$(remote_run snapshot)
revision=$(remote_run helm-upgrade DRY_RUN=0 < "$tmp/bundle.tar" | jq -r '.version')
after=$(remote_run snapshot)
if [ -f "$CFG/state/secret-restart-pending" ]; then
  if [ "$before" = "$after" ]; then remote_run restart >&2; fi
  rm "$CFG/state/secret-restart-pending"
fi
jq -nc --arg time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg commit "$version" --arg digest "$digest" --arg revision "$revision" '{time:$time,commit:$commit,digest:$digest,revision:$revision,action:"rollout",result:"success"}' >> "$CFG/state/deployments.jsonl"
printf '%s\n' "$revision"
