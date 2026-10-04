#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/../lib/config.sh"
source "$SKILL_DIR/scripts/lib/registry.sh"
[ "$#" -eq 0 ] || config_error 'publish takes no arguments'
cd "$REPO_ROOT"
[ -z "$(git status --porcelain)" ] || config_error 'publish requires a clean work tree'
commit=$(git rev-parse --short=12 HEAD)
image_ref="$REGISTRY_PUSH/$IMAGE_PUSH_PATH:$commit"
base_ref="$REGISTRY_PUSH/$BASE_PUSH_PATH:$(base_tag)"
base_digest=''
work=''
local_tag=''
record=''
cleanup() {
  [ -z "$record" ] || rm -f "$record"
  [ -z "$work" ] || rm -rf "$work"
  [ -z "$local_tag" ] || docker image rm "$local_tag" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
image_status=0
image_digest=$(read_digest "$image_ref") || image_status=$?
if [ "$image_status" = 0 ]; then
  base_status=0
  base_digest=$(read_digest "$base_ref") || base_status=$?
  case "$base_status" in 0) ;; 3) base_digest='' ;; *) exit 1 ;; esac
else
  [ "$image_status" = 3 ] || exit 1
  base_status=0
  base_digest=$(read_digest "$base_ref") || base_status=$?
  [ "$base_status" = 0 ] || [ "$base_status" = 3 ] || exit 1
  if [ "$base_status" = 3 ]; then
    umask 077
    work=$(mktemp -d)
    local_tag="ferry-base:$(base_tag)"
    docker build --platform linux/amd64 --provenance=false -f docker/base.Dockerfile -t "$local_tag" . >&2
    docker save --platform linux/amd64 -o "$work/base.tar" "$local_tag" >&2
    crane_cmd "$REGISTRY_INSECURE" push "$work/base.tar" "$base_ref" >&2
    base_digest=$(read_digest "$base_ref") || { echo 'publish: registry requires credentials or is unavailable' >&2; exit 1; }
  fi
  crane_cmd "$REGISTRY_INSECURE" config --platform linux/amd64 "$REGISTRY_PUSH/$BASE_PUSH_PATH@$base_digest" |
    jq -e '.os == "linux" and .architecture == "amd64"' >/dev/null
  crane_cmd "$REGISTRY_INSECURE" manifest "$REGISTRY_PUSH/$BASE_PUSH_PATH@$base_digest" |
    jq -e 'has("manifests") | not' >/dev/null
  FERRY_GIT_SHA="$commit" "${FERRY_DEPLOY_BUILD_CMD:-scripts/build-binaries.sh}" >&2
  if [ "$REGISTRY_INSECURE" = true ]; then
    assembled=$(FERRY_INSECURE_REGISTRY="$REGISTRY_PUSH" "${FERRY_DEPLOY_ASSEMBLE_CMD:-scripts/assemble-image.sh}" "$REGISTRY_PUSH/$BASE_PUSH_PATH@$base_digest" "$REGISTRY_PUSH/$IMAGE_PUSH_PATH" "$commit")
  else
    assembled=$(FERRY_INSECURE_REGISTRY='' "${FERRY_DEPLOY_ASSEMBLE_CMD:-scripts/assemble-image.sh}" "$REGISTRY_PUSH/$BASE_PUSH_PATH@$base_digest" "$REGISTRY_PUSH/$IMAGE_PUSH_PATH" "$commit")
  fi
  image_digest=$(read_digest "$image_ref")
  [ "$image_digest" = "$(printf '%s\n' "$assembled" | tail -n 1)" ] || { echo 'publish: digest mismatch' >&2; exit 1; }
fi
umask 077
record=$(mktemp "$CFG/state/publish.XXXXXX")
jq -n --arg commit "$commit" --arg image_digest "$image_digest" --arg base_digest "$base_digest" --arg time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" '{commit:$commit,image_digest:$image_digest,base_digest:$base_digest,time:$time}' >"$record"
mv "$record" "$CFG/state/last-publish.json"
printf '%s\n' "$image_digest"
