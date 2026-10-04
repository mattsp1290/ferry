#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/../lib/config.sh"
source "$SKILL_DIR/scripts/lib/values.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/secret.sh"
source "$SKILL_DIR/scripts/lib/namespace.sh"
umask 077
rm -f "$CFG/state/preflight.yaml" "$CFG/state/preflight.json"
digest= version=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --digest|--version)
      [ "$#" -ge 2 ] || config_error 'missing preflight option value'
      key=$1; shift
      if [ "$key" = --digest ]; then digest=$1; else version=$1; fi;;
    *) config_error 'usage: deploy preflight [--digest sha256:... --version 12-hex]';;
  esac
  shift
done
if [ -z "$digest" ] && [ -z "$version" ]; then
  [ -f "$CFG/state/last-publish.json" ] || config_error 'run deploy publish first'
  digest=$(jq -r '.image_digest' "$CFG/state/last-publish.json")
  version=$(jq -r '.commit' "$CFG/state/last-publish.json")
fi
[[ "$digest" =~ ^sha256:[a-f0-9]{64}$ ]] && [[ "$version" =~ ^[a-f0-9]{12}$ ]] || config_error 'digest and version must both be supplied and valid'
fingerprint=$(preflight_values_fingerprint)
values=$(merged_values)
owner=$(owner_values_json)
printf '%s' "$values" | jq -e '.datadog.transport as $transport | ["socket","service","none"] | index($transport) != null' >/dev/null || config_error 'datadog.transport must be socket, service or none'
printf '%s' "$values" | jq -e '.nodeSelector | type=="object" and all(.[]; type=="string")' >/dev/null || config_error 'nodeSelector must map label keys to string values'
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-deploy.XXXXXX")
cleanup() {
  rm -rf "$tmp"
  if [ -f "$CFG/state/secret-restart-pending" ]; then
    printf 'credentials changed and the pod has not reloaded them: run deploy restart\n' >&2
  fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
if namespace_guard; then :; else
  guard_result=$?
  if [ "$guard_result" = 3 ]; then remote_run namespace-create; else exit 1; fi
fi
apply_secret >&2
# Command arguments and environment are serialized by jq; no live value is
# inserted into the shell program executed in the container.
cat > "$tmp/probe.sh" <<'PROBE'
check() { if "$@" >/dev/null 2>&1; then printf 'ok:%s\n' "$label"; else printf 'FAIL:%s\n' "$label"; fi; }
label=version; ferry --version | grep -F "$EXPECTED_COMMIT" >/dev/null && echo ok:version || echo FAIL:version
label=lfs; check git lfs version
label=token; check test -s /var/run/secrets/ferry/forgejo-token
label=pvc; check sh -c 'touch /var/lib/ferry/probe && rm /var/lib/ferry/probe'
# git's auth failure proves the TLS handshake completed. Disable credentials
# and prompts, and never print the response (which may include repository data).
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
attempt=0
while :; do
  attempt=$((attempt + 1))
  pod_name="ferry-preflight-$(date +%s)-$$-$attempt"
  jq --arg name "$pod_name" --argjson v "$values" --arg image "$IMAGE_PULL_REPOSITORY@$digest" --arg version "$version" --argjson fix "$fix" --argjson socket_probe "$socket_probe" --rawfile probe "$tmp/probe.sh" '
  walk(if type=="string" then
    if .=="@NAME@" then $name elif .=="@IMAGE@" then $image elif .=="@VERSION@" then $version
    elif .=="@PROBE@" then $probe elif .=="@SECRET@" then $v.credentialsSecret
    elif .=="@SOCKET_PATH@" then $v.datadog.socketHostPath
    elif .=="@FORGEJO_URL@" then ($v.config.forgejo.url|rtrimstr("/"))+"/"+$v.config.repos[0].forgejo+".git"
    elif .=="@AGENT_SERVICE@" then $v.datadog.agentService
    elif .=="@STORAGE_CLASS@" then $v.cache.storageClass else . end else . end)
  | .items |= map(.metadata.labels["ferry-deploy-preflight"]=$name)
  | .items[1].spec.nodeSelector=$v.nodeSelector
  | if ($socket_probe|not) then (.items[1].spec.volumes |= map(select(.name != "sockets"))) | (.items[1].spec.containers[0].volumeMounts |= map(select(.name != "sockets"))) else . end
  | if $fix then .items[1].spec.initContainers=[{name:"fix-permissions",image:$image,command:["chown","10001:10001","/var/lib/ferry"],securityContext:{runAsNonRoot:false,runAsUser:0,runAsGroup:0,readOnlyRootFilesystem:true,allowPrivilegeEscalation:false,capabilities:{drop:["ALL"],add:["CHOWN"]}},resources:{requests:{cpu:"10m",memory:"16Mi"},limits:{cpu:"100m",memory:"64Mi"}},volumeMounts:[{name:"cache",mountPath:"/var/lib/ferry"}]}] else . end' "$SKILL_DIR/resources/preflight-pod.yaml" > "$tmp/pod.json"
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
if grep -qx 'ok:sockets' "$tmp/results"; then transport=socket; elif grep -qx 'ok:service' "$tmp/results"; then transport=service; fi
if printf '%s' "$owner" | jq -e --argjson sockets "$sockets" --argjson service "$service" '(.datadog.transport=="socket" and ($sockets|not)) or (.datadog.transport=="service" and ($service|not))' >/dev/null; then echo 'values.yaml datadog.transport is unworkable; update the owner value' >&2; exit 1; fi
if [ "$fix" = true ] && printf '%s' "$owner" | jq -e '.cache.fixPermissions==false' >/dev/null; then echo 'values.yaml cache.fixPermissions is unworkable; update the owner value' >&2; exit 1; fi
printf 'cache:\n  fixPermissions: %s\ndatadog:\n  transport: %s\n' "$fix" "$transport" > "$CFG/state/preflight.yaml"
jq -nc --arg namespace "$NAMESPACE" --arg release "$RELEASE" --arg digest "$digest" --arg version "$version" --arg control_ssh "$CONTROL_SSH" --arg kubeconfig_path "$KUBECONFIG_PATH" --arg image_pull_repository "$IMAGE_PULL_REPOSITORY" --arg values_sha256 "$fingerprint" '{namespace:$namespace,release:$release,digest:$digest,version:$version,control_ssh:$control_ssh,kubeconfig_path:$kubeconfig_path,image_pull_repository:$image_pull_repository,values_sha256:$values_sha256}' > "$CFG/state/preflight.json"
