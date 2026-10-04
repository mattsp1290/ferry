#!/usr/bin/env bash
# Config has already validated all settings interpolated into the login shell.
remote_run() {
  local name=$1 encoded command setting extra
  shift
  case "$name" in *[!a-z-]*|'') config_error 'invalid remote script name';; esac
  encoded=$(base64 < "$SKILL_DIR/scripts/remote/$name.sh" | tr -d '\r\n')
  command='env'
  for setting in KUBECONFIG_PATH NAMESPACE RELEASE HELM_TIMEOUT NODE_SELECTOR IMAGE_PULL_REPOSITORY; do
    command="$command $setting=${!setting}"
  done
  for extra in "$@"; do
    case "$extra" in [A-Z_]*=*) ;; *) config_error 'invalid remote setting';; esac
    # Additional settings have a deliberately narrower alphabet than deploy.env.
    case "$extra" in *[!A-Za-z0-9_./:=@-]*) config_error 'invalid remote setting';; esac
    command="$command $extra"
  done
  command="$command bash -c \"\$(printf %s $encoded | base64 -d)\""
  ssh -o BatchMode=yes "$CONTROL_SSH" -- "$command"
}
