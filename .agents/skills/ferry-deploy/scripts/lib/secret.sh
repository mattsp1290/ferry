#!/usr/bin/env bash
# Read token bytes only through descriptors and pipes, never shell variables.
secret_manifest() {
  local values secret_name
  values=$(merged_values)
  secret_name=$(printf '%s' "$values" | jq -r '.credentialsSecret')
  with_token_fd "$CFG/secrets/forgejo-token" 8 bash -c 'python3 -c '\''import sys,base64; sys.stdout.buffer.write(base64.b64encode(sys.stdin.buffer.read().decode("utf-8").rstrip().encode("utf-8")))'\'' < /dev/fd/8' |
    jq -Rs --arg namespace "$NAMESPACE" --arg name "$secret_name" '{apiVersion:"v1",kind:"Secret",metadata:{name:$name,namespace:$namespace},type:"Opaque",data:{"forgejo-token":.}}' |
    if [ -s "$CFG/secrets/github-token" ]; then
      # jq receives the second token through its descriptor, never an argument.
      with_token_fd "$CFG/secrets/github-token" 8 bash -c 'exec 9< <(python3 -c '\''import sys,base64; sys.stdout.buffer.write(base64.b64encode(sys.stdin.buffer.read().decode("utf-8").rstrip().encode("utf-8")))'\'' < /dev/fd/8); jq --rawfile github /dev/fd/9 '\''.data += (if $github == "" then {} else {"github-token":$github} end)'\'''
    else
      cat
    fi
}
apply_secret() {
  local result secret_name
  secret_name=$(merged_values | jq -r '.credentialsSecret')
  case "$secret_name" in *[!a-z0-9.-]*|'') config_error 'invalid credentialsSecret';; esac
  result=$(secret_manifest | remote_run apply-secret "SECRET_NAME=$secret_name") || return 1
  case "$result" in created|changed) : > "$CFG/state/secret-restart-pending";; unchanged) ;; *) printf 'invalid Secret apply result\n' >&2; return 1;; esac
  printf '%s\n' "$result"
}
