#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/../lib/config.sh"
source "$SKILL_DIR/scripts/lib/values.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/secret.sh"
source "$SKILL_DIR/scripts/lib/namespace.sh"
source "$SKILL_DIR/scripts/lib/state.sh"
umask 077
explicit_image=$#
resolve_image "$@"
cd "$REPO_ROOT"
[ -z "$(git status --porcelain)" ] || config_error 'rollout requires a clean work tree'
if [ "$explicit_image" = 0 ] && [ "$version" != "$(git rev-parse --short=12 HEAD)" ]; then
  config_error 'run deploy publish first: checked-out commit differs'
fi
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-deploy.XXXXXX")
revision= completed=false
cleanup() {
  code=$?
  trap - EXIT
  rm -rf "$tmp"
  warn_restart_pending
  if [ "$completed" != true ]; then
    log_deployment rollout failed "$revision"
  fi
  exit "$code"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
[ -f "$CFG/state/preflight.json" ] && [ -f "$CFG/state/preflight.yaml" ] || config_error 'run deploy preflight first'
[ "$(preflight_binding)" = "$(jq -cS . "$CFG/state/preflight.json")" ] || config_error 'run deploy preflight first: target or image differs'
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
log_deployment rollout success "$revision"
completed=true
printf '%s\n' "$revision"
