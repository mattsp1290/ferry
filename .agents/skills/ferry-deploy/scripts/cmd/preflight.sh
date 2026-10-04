#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/../lib/config.sh"
source "$SKILL_DIR/scripts/lib/values.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/secret.sh"
source "$SKILL_DIR/scripts/lib/namespace.sh"
source "$SKILL_DIR/scripts/lib/state.sh"
umask 077
rm -f "$CFG/state/preflight.yaml" "$CFG/state/preflight.json"
resolve_image "$@"
values=$(merged_values)
owner=$(owner_values_json)
printf '%s' "$values" | jq -e '.nodeSelector | type=="object" and all(.[]; type=="string")' >/dev/null || config_error 'nodeSelector must map label keys to string values'
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-deploy.XXXXXX")
cleanup() {
  rm -rf "$tmp"
  warn_restart_pending
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
# Helm serializes the probe program and values as data; no live value is
# inserted into the shell program executed in the container.
cat > "$tmp/probe.sh" <<'PROBE'
check() { if "$@" >/dev/null 2>&1; then printf 'ok:%s\n' "$label"; else printf 'FAIL:%s\n' "$label"; fi; }
label=version; ferry --version | grep -F "$EXPECTED_COMMIT" >/dev/null && echo ok:version || echo FAIL:version
label=lfs; check git lfs version
label=token; check test -s /var/run/secrets/ferry/forgejo-token
label=pvc; check sh -c 'touch /var/lib/ferry/probe && rm /var/lib/ferry/probe'
# git's auth failure proves the configured Forgejo route responds. HTTPS
# also verifies TLS; an internal HTTP route requires the owner's explicit opt-in.
# Disable credentials and prompts, and never print repository responses.
git -c credential.helper= ls-remote "$FORGEJO_URL" >/tmp/tls-out 2>/tmp/tls-error && echo ok:tls || {
  sed "s/'[^']*'//g" /tmp/tls-error > /tmp/tls-classification
  if grep -Ei 'authentication failed|could not read Username|repository .* not found|not found|403|401' /tmp/tls-classification >/dev/null && ! grep -Ei 'SSL|certificate|resolve|connect|TLS' /tmp/tls-classification >/dev/null; then echo ok:tls; else echo FAIL:tls; fi
}
label=sockets; check sh -c 'test -S /var/run/datadog/dsd.socket && test -w /var/run/datadog/dsd.socket && test -S /var/run/datadog/apm.socket && test -w /var/run/datadog/apm.socket'
label=service; check getent hosts "$AGENT_SERVICE"
PROBE
fix=false
socket_probe=$(printf '%s' "$values" | jq -r '.datadog.transport=="socket"')
owner_socket=$(printf '%s' "$owner" | jq -r '.datadog.transport=="socket"')
attempt=0 initialized=false
while :; do
  attempt=$((attempt + 1))
  pod_name="ferry-preflight-$(date +%s)-$$-$attempt"
  probe_transport=$(printf '%s' "$values" | jq -r '.datadog.transport')
  if [ "$probe_transport" = socket ] && [ "$socket_probe" != true ]; then probe_transport=none; fi
  helm template "$RELEASE" "$REPO_ROOT/charts/ferry" -f "$CFG/values.yaml" \
    --set-string "image.repository=$IMAGE_PULL_REPOSITORY" --set-string "image.digest=$digest" --set-string "image.version=$version"     --set-string "preflight.name=$pod_name" --set-file "preflight.probe=$tmp/probe.sh" \
    --set "cache.fixPermissions=$fix" --set "datadog.transport=$probe_transport" \
    --show-only templates/preflight.yaml | sed '/^---$/d; /^# Source:/d' > "$tmp/pod.json"
  if [ "$initialized" = false ]; then
    if namespace_guard; then :; else
      guard_result=$?
      if [ "$guard_result" = 3 ]; then remote_run namespace-create; else exit 1; fi
    fi
    apply_secret >&2
    initialized=true
  fi
  if remote_run preflight-pod "POD_NAME=$pod_name" < "$tmp/pod.json" > "$tmp/results"; then :; else
    probe_result=$?
    if [ "$probe_result" = 4 ] && [ "$socket_probe" = true ] && [ "$owner_socket" != true ]; then
      printf 'socket directory unavailable; probing Service transport without hostPath\n' >&2
      socket_probe=false
      continue
    fi
    cat "$tmp/results" >&2
    exit 1
  fi
  cat "$tmp/results" >&2
  grep -qx 'phase:Succeeded' "$tmp/results" || { echo 'preflight pod did not complete successfully' >&2; exit 1; }
  for check in version lfs token tls; do
    if ! grep -qx "ok:$check" "$tmp/results"; then printf 'preflight failed: %s\n' "$check" >&2; exit 1; fi
  done
  if grep -qx 'ok:pvc' "$tmp/results"; then break; fi
  [ "$fix" = false ] || { echo 'preflight PVC remains unwritable after permission repair' >&2; exit 1; }
  fix=true
done
sockets=false service=false
grep -qx 'ok:sockets' "$tmp/results" && sockets=true
grep -qx 'ok:service' "$tmp/results" && service=true
transport=none
if [ "$sockets" = true ]; then transport=socket; elif [ "$service" = true ]; then transport=service; fi
if printf '%s' "$owner" | jq -e --argjson sockets "$sockets" --argjson service "$service" '(.datadog.transport=="socket" and ($sockets|not)) or (.datadog.transport=="service" and ($service|not))' >/dev/null; then echo 'values.yaml datadog.transport is unworkable; update the owner value' >&2; exit 1; fi
if [ "$fix" = true ] && printf '%s' "$owner" | jq -e '.cache.fixPermissions==false' >/dev/null; then echo 'values.yaml cache.fixPermissions is unworkable; update the owner value' >&2; exit 1; fi
printf 'cache:\n  fixPermissions: %s\ndatadog:\n  transport: %s\n' "$fix" "$transport" > "$CFG/state/preflight.yaml"
preflight_binding > "$CFG/state/preflight.json"
