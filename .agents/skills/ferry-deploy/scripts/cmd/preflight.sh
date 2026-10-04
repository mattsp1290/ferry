#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/../lib/config.sh"
source "$SKILL_DIR/scripts/lib/values.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/secret.sh"
[ "$#" = 0 ] || config_error 'usage: deploy preflight'
umask 077
[ -f "$CFG/state/last-publish.json" ] || config_error 'run deploy publish first'
digest=$(jq -r '.image_digest' "$CFG/state/last-publish.json")
version=$(jq -r '.commit' "$CFG/state/last-publish.json")
[[ "$digest" =~ ^sha256:[a-f0-9]{64}$ ]] && [[ "$version" =~ ^[a-f0-9]{12}$ ]] || config_error 'invalid published image'
values=$(merged_values)
owner=$(owner_values_json)
releases=$(remote_run ownership)
if printf '%s' "$releases" | jq -e --arg r "$RELEASE" --arg n "$NAMESPACE" 'any(.[]; .name==$r and .namespace!=$n)' >/dev/null; then echo 'release exists outside target namespace' >&2; exit 1; fi
namespace=$(remote_run namespace)
if [ -n "$namespace" ]; then
  if ! printf '%s' "$namespace" | jq -e '.metadata.labels["app.kubernetes.io/managed-by"]=="ferry-deploy"' >/dev/null && ! printf '%s' "$releases" | jq -e --arg r "$RELEASE" --arg n "$NAMESPACE" 'any(.[]; .name==$r and .namespace==$n)' >/dev/null; then echo 'namespace is not owned by ferry-deploy or the release' >&2; exit 1; fi
else
  remote_run namespace-create
fi
apply_secret >&2
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-deploy.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM
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
  if grep -Ei 'authentication failed|could not read Username|repository .* not found|not found|403|401' /tmp/tls-error >/dev/null && ! grep -Ei 'SSL|certificate|resolve|connect|TLS' /tmp/tls-error >/dev/null; then echo ok:tls; else echo FAIL:tls; fi
}
label=sockets; check sh -c 'test -S /var/run/datadog/dsd.socket && test -w /var/run/datadog/dsd.socket && test -S /var/run/datadog/apm.socket && test -w /var/run/datadog/apm.socket'
label=service; check getent hosts "$AGENT_SERVICE"
PROBE
fix=false
attempt=0
while :; do
  attempt=$((attempt + 1))
  pod_name="ferry-preflight-$(date +%s)-$$-$attempt"
  jq --arg name "$pod_name" --argjson v "$values" --arg image "$IMAGE_PULL_REPOSITORY@$digest" --arg version "$version" --arg selector "$NODE_SELECTOR" --argjson fix "$fix" --rawfile probe "$tmp/probe.sh" '
  walk(if type=="string" then
    if .=="@NAME@" then $name elif .=="@IMAGE@" then $image elif .=="@VERSION@" then $version
    elif .=="@PROBE@" then $probe elif .=="@SECRET@" then $v.credentialsSecret
    elif .=="@SOCKET_PATH@" then $v.datadog.socketHostPath
    elif .=="@FORGEJO_URL@" then ($v.config.forgejo.url|rtrimstr("/"))+"/"+$v.config.repos[0].forgejo+".git"
    elif .=="@AGENT_SERVICE@" then $v.datadog.agentService
    elif .=="@STORAGE_CLASS@" then $v.cache.storageClass else . end else . end)
  | .items |= map(.metadata.labels["ferry-deploy-preflight"]=$name)
  | .items[1].spec.nodeSelector=($selector|split("=")|{(.[0]):.[1]})
  | if $fix then .items[1].spec.initContainers=[{name:"fix-permissions",image:$image,command:["chown","10001:10001","/var/lib/ferry"],securityContext:{runAsNonRoot:false,runAsUser:0,runAsGroup:0,readOnlyRootFilesystem:true,allowPrivilegeEscalation:false,capabilities:{drop:["ALL"],add:["CHOWN"]}},resources:{requests:{cpu:"10m",memory:"16Mi"},limits:{cpu:"100m",memory:"64Mi"}},volumeMounts:[{name:"cache",mountPath:"/var/lib/ferry"}]}] else . end' "$SKILL_DIR/resources/preflight-pod.yaml" > "$tmp/pod.json"
  remote_run preflight-pod "POD_NAME=$pod_name" < "$tmp/pod.json" > "$tmp/results"
  cat "$tmp/results" >&2
  grep -qx 'phase:Succeeded' "$tmp/results" || { echo 'preflight pod did not complete successfully' >&2; exit 1; }
  for check in version lfs token tls; do
    if ! grep -qx "ok:$check" "$tmp/results"; then printf 'preflight failed: %s\n' "$check" >&2; exit 1; fi
  done
  if grep -qx 'ok:pvc' "$tmp/results"; then break; fi
  [ "$attempt" = 1 ] || { echo 'preflight PVC remains unwritable after permission repair' >&2; exit 1; }
  fix=true
done
transport=none
if grep -qx 'ok:sockets' "$tmp/results"; then transport=socket; elif grep -qx 'ok:service' "$tmp/results"; then transport=service; fi
if printf '%s' "$owner" | jq -e --arg t "$transport" '(.datadog.transport=="socket" and $t!="socket") or (.datadog.transport=="service" and $t=="none")' >/dev/null; then echo 'values.yaml datadog.transport is unworkable; update the owner value' >&2; exit 1; fi
if [ "$fix" = true ] && printf '%s' "$owner" | jq -e '.cache.fixPermissions==false' >/dev/null; then echo 'values.yaml cache.fixPermissions is unworkable; update the owner value' >&2; exit 1; fi
printf 'cache:\n  fixPermissions: %s\ndatadog:\n  transport: %s\n' "$fix" "$transport" > "$CFG/state/preflight.yaml"
