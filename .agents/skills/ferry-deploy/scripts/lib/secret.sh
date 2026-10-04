#!/usr/bin/env bash
# Read token bytes only through descriptors and pipes, never shell variables.
secret_manifest() {
  local secret_name github=/dev/null
  secret_name=$(merged_values | jq -r '.credentialsSecret')
  [ ! -e "$CFG/secrets/github-token" ] || github="$CFG/secrets/github-token"
  with_token_fd "$CFG/secrets/forgejo-token" 8 with_token_fd "$github" 9 \
    python3 "$SKILL_DIR/scripts/lib/secret-manifest.py" "$NAMESPACE" "$secret_name" "$@"
}
apply_secret() {
  local result secret_name command
  secret_name=$(merged_values | jq -r '.credentialsSecret')
  case "$secret_name" in *[!a-z0-9.-]*|'') config_error 'invalid credentialsSecret';; esac
  command=$(remote_command apply-secret "SECRET_NAME=$secret_name")
  result=$(secret_manifest "$CONTROL_SSH" "$command") || return 1
  case "$result" in created|changed) : > "$CFG/state/secret-restart-pending";; unchanged) ;; *) printf 'invalid Secret apply result\n' >&2; return 1;; esac
  printf '%s\n' "$result"
}
