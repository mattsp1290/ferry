#!/usr/bin/env bash
# Shared image identity, probe binding and local deployment records.
resolve_image() {
  local option_count=$# key
  digest= version=
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --digest|--version)
        [ "$#" -ge 2 ] || config_error 'missing image option value'
        key=$1
        shift
        if [ "$key" = --digest ]; then digest=$1; else version=$1; fi ;;
      *) config_error 'usage: deploy preflight|rollout [--digest sha256:... --version 12-hex]' ;;
    esac
    shift
  done
  if [ "$option_count" = 0 ]; then
    [ -f "$CFG/state/last-publish.json" ] || config_error 'run deploy publish first'
    digest=$(jq -r '.image_digest' "$CFG/state/last-publish.json")
    version=$(jq -r '.commit' "$CFG/state/last-publish.json")
  fi
  [[ "$digest" =~ ^sha256:[a-f0-9]{64}$ ]] && [[ "$version" =~ ^[a-f0-9]{12}$ ]] || config_error 'digest and version must both be supplied and valid'
}
preflight_binding() {
  jq -ncS --arg namespace "$NAMESPACE" --arg release "$RELEASE" --arg digest "$digest" --arg version "$version" \
    --arg control_ssh "$CONTROL_SSH" --arg kubeconfig_path "$KUBECONFIG_PATH" \
    --arg image_pull_repository "$IMAGE_PULL_REPOSITORY" --arg values_sha256 "$(preflight_values_fingerprint)" '$ARGS.named'
}
warn_restart_pending() {
  if [ -f "$CFG/state/secret-restart-pending" ]; then
    printf 'credentials changed and the pod has not reloaded them: run deploy restart\n' >&2
  fi
}
log_deployment() {
  jq -nc --arg time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg commit "$version" --arg digest "$digest" \
    --arg action "$1" --arg result "$2" --arg revision "$3" '$ARGS.named' >> "$CFG/state/deployments.jsonl"
}
