#!/usr/bin/env bash
# Config has already validated all settings interpolated into the login shell.
remote_command() {
  local name=$1 encoded command setting extra
  shift
  case "$name" in *[!a-z-]*|'') config_error 'invalid remote script name';; esac
  encoded=$(cat "$SKILL_DIR/scripts/remote/lib.sh" "$SKILL_DIR/scripts/remote/$name.sh" | base64 | tr -d '\r\n')
  command='env'
  for setting in KUBECONFIG_PATH NAMESPACE RELEASE HELM_TIMEOUT; do
    command="$command $setting=${!setting}"
  done
  for extra in "$@"; do
    case "$extra" in [A-Z_]*=*) ;; *) config_error 'invalid remote setting';; esac
    # Additional settings have a deliberately narrower alphabet than deploy.env.
    case "$extra" in *[!A-Za-z0-9_./:=@-]*) config_error 'invalid remote setting';; esac
    command="$command $extra"
  done
  command="$command bash -c \"\$(printf %s $encoded | base64 -d)\""
  printf '%s\n' "$command"
}
remote_run() {
  local command
  command=$(remote_command "$@")
  ssh -o BatchMode=yes -o ConnectTimeout=15 "$CONTROL_SSH" -- "$command"
}
