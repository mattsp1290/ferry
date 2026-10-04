#!/usr/bin/env bash
set -euo pipefail
umask 077
skill=$(cd "$(dirname "$0")/.." && pwd)
source "$skill/tests/lib/stubs.sh"
stubs_init
trap 'rm -rf "$STUB_ROOT"' EXIT
entry="$skill/scripts/deploy.sh"
bash "$entry" publish >/dev/null
apply_count() {
  python3 - "$STUB_ROOT/records" <<'PYCOUNT'
import json,pathlib,sys
print(sum(1 for p in pathlib.Path(sys.argv[1]).glob('*.json') if (lambda r:r['tool']=='kubectl' and 'apply' in r['argv'])(json.loads(p.read_text()))))
PYCOUNT
}
refuse_without_apply() {
  local before
  before=$(apply_count)
  if bash "$entry" "$@" > "$STUB_ROOT/refused.out" 2>&1; then echo 'unsafe deployment accepted' >&2; exit 1; fi
  [ "$(apply_count)" = "$before" ]
}
# Neither an absent preflight nor a rejected namespace may rotate credentials.
refuse_without_apply rollout
grep -q 'run deploy preflight first' "$STUB_ROOT/refused.out"
export STUB_NAMESPACE_UNOWNED=1
if bash "$entry" preflight > "$STUB_ROOT/unowned.out" 2>&1; then echo 'unowned namespace accepted' >&2; exit 1; fi
python3 - "$STUB_ROOT/records" <<'PY'
import json,pathlib,sys
assert not any('apply' in json.loads(p.read_text())['argv'] for p in pathlib.Path(sys.argv[1]).glob('*.json'))
PY
unset STUB_NAMESPACE_UNOWNED
export STUB_PREFLIGHT_PVC_FAIL_ONCE=1
bash "$entry" preflight >/dev/null 2>&1
grep -q 'fixPermissions: true' "$FERRY_DEPLOY_CONFIG/state/preflight.yaml"
[ -f "$FERRY_DEPLOY_CONFIG/state/secret-restart-pending" ]
[ -f "$FERRY_DEPLOY_CONFIG/state/preflight.json" ]
jq -e --arg d "$STUB_DIGEST" '.namespace=="ferry" and .release=="ferry" and .digest==$d' "$FERRY_DEPLOY_CONFIG/state/preflight.json" >/dev/null
cp "$FERRY_DEPLOY_CONFIG/state/preflight.json" "$STUB_ROOT/preflight.json"
export STUB_NAMESPACE_UNOWNED=1
refuse_without_apply rollout
unset STUB_NAMESPACE_UNOWNED
for key in namespace release digest control_ssh kubeconfig_path image_pull_repository values_sha256; do
  jq --arg key "$key" '.[$key]="different"' "$STUB_ROOT/preflight.json" > "$FERRY_DEPLOY_CONFIG/state/preflight.json"
  refuse_without_apply rollout
  grep -q 'run deploy preflight first' "$STUB_ROOT/refused.out"
done
cp "$STUB_ROOT/preflight.json" "$FERRY_DEPLOY_CONFIG/state/preflight.json"
cp "$FERRY_DEPLOY_CONFIG/values.yaml" "$STUB_ROOT/saved-values.yaml"
printf '\n# changed since probe\n' >> "$FERRY_DEPLOY_CONFIG/values.yaml"
refuse_without_apply rollout
cp "$STUB_ROOT/saved-values.yaml" "$FERRY_DEPLOY_CONFIG/values.yaml"
cp "$FERRY_DEPLOY_CONFIG/deploy.env" "$STUB_ROOT/saved-deploy.env"
sed 's/admin@control.example.internal/admin@other.example.internal/' "$STUB_ROOT/saved-deploy.env" > "$FERRY_DEPLOY_CONFIG/deploy.env"
refuse_without_apply rollout
cp "$STUB_ROOT/saved-deploy.env" "$FERRY_DEPLOY_CONFIG/deploy.env"

for flag in STUB_LINT_FAIL STUB_DRY_RUN_FAIL; do
  export "$flag=1" STUB_SECRET_CHANGED=1
  refuse_without_apply rollout
  tail -n 1 "$FERRY_DEPLOY_CONFIG/state/deployments.jsonl" | jq -e '.result=="failed"' >/dev/null
  unset "$flag" STUB_SECRET_CHANGED
done
unset STUB_PREFLIGHT_PVC_FAIL_ONCE
export STUB_KEEP_UID=1 STUB_DRY_SECRET=DO-NOT-ECHO-RENDERED-SECRET
bash "$entry" rollout > "$STUB_ROOT/rollout.out" 2>&1
if grep -q DO-NOT-ECHO-RENDERED-SECRET "$STUB_ROOT/rollout.out"; then exit 1; fi
[ ! -f "$FERRY_DEPLOY_CONFIG/state/secret-restart-pending" ]
restart_count() {
  python3 - "$STUB_ROOT/records" <<'PY'
import json,pathlib,sys
print(sum(1 for p in pathlib.Path(sys.argv[1]).glob('*.json') if (lambda r:r['tool']=='kubectl' and 'restart' in r['argv'])(json.loads(p.read_text()))))
PY
}
[ "$(restart_count)" = 1 ]
# An unchanged Secret with no pending marker causes no extra restart.
bash "$entry" rollout >/dev/null 2>&1
[ "$(restart_count)" = 1 ]
# A changed Secret applied in preflight survives the following unchanged apply.
export STUB_SECRET_CHANGED=1
bash "$entry" preflight > "$STUB_ROOT/rotation-preflight.out" 2>&1
grep -qx changed "$STUB_ROOT/rotation-preflight.out"
rotation_version=$(cat "$STUB_ROOT/secret-present")
unset STUB_SECRET_CHANGED
bash "$entry" rollout > "$STUB_ROOT/rotation-rollout.out" 2>&1
grep -qx unchanged "$STUB_ROOT/rotation-rollout.out"
[ "$(cat "$STUB_ROOT/secret-present")" = "$rotation_version" ]
[ "$(restart_count)" = 2 ]
# A replaced pod clears the pending marker without another restart.
unset STUB_KEEP_UID
rm "$STUB_ROOT/upgraded"
: > "$FERRY_DEPLOY_CONFIG/state/secret-restart-pending"
bash "$entry" rollout >/dev/null 2>&1
[ "$(restart_count)" = 2 ]
[ ! -f "$FERRY_DEPLOY_CONFIG/state/secret-restart-pending" ]
# Failed upgrade preserves the marker; the next successful invocation restarts.
export STUB_KEEP_UID=1 STUB_SECRET_CHANGED=1 STUB_UPGRADE_FAIL=1
if bash "$entry" rollout > "$STUB_ROOT/interrupted.out" 2>&1; then echo 'failed upgrade accepted' >&2; exit 1; fi
[ -f "$FERRY_DEPLOY_CONFIG/state/secret-restart-pending" ]
grep -q 'credentials changed and the pod has not reloaded them: run deploy restart' "$STUB_ROOT/interrupted.out"
tail -n 1 "$FERRY_DEPLOY_CONFIG/state/deployments.jsonl" | jq -e '.result=="failed"' >/dev/null
unset STUB_SECRET_CHANGED STUB_UPGRADE_FAIL
bash "$entry" rollout >/dev/null 2>&1
[ "$(restart_count)" = 3 ]
# All-digit commit IDs stay strings in both serialized values and Helm output.
external_digest=sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
refuse_without_apply rollout --digest "$external_digest" --version 123456789012
bash "$entry" preflight --digest "$external_digest" --version 123456789012 >/dev/null 2>&1
jq -e --arg d "$external_digest" '.digest==$d and .version=="123456789012"' "$FERRY_DEPLOY_CONFIG/state/preflight.json" >/dev/null
bash "$entry" rollout --digest "$external_digest" --version 123456789012 >/dev/null 2>&1
python3 - "$STUB_ROOT/records" "$STUB_ROOT/image.yaml" <<'PYIMAGE'
import io,pathlib,sys,tarfile
found=False
for p in pathlib.Path(sys.argv[1]).glob('*.stdin'):
    try:
        with tarfile.open(fileobj=io.BytesIO(p.read_bytes())) as tar:
            data=tar.extractfile('image.yaml').read()
            if b'123456789012' in data:
                assert b'version: "123456789012"' in data
                pathlib.Path(sys.argv[2]).write_bytes(data);found=True
    except tarfile.ReadError: pass
assert found
PYIMAGE
"$STUB_REAL_HELM" template values-reader "$skill/resources/values-reader" -f "$STUB_ROOT/image.yaml" | sed '/^---$/d; /^# Source:/d' | jq -e '.image.version=="123456789012"' >/dev/null
# Probe selectors follow the complete merged chart values. Disabled/service
# transports do not create or mount a host socket directory.
cp "$FERRY_DEPLOY_CONFIG/values.yaml" "$STUB_ROOT/owner-values.yaml"
printf '\nnodeSelector:\n  kubernetes.io/arch: arm64\n  topology.kubernetes.io/zone: example-zone\n' >> "$FERRY_DEPLOY_CONFIG/values.yaml"
bash "$entry" preflight > "$STUB_ROOT/selector-preflight.out" 2>&1
python3 - "$STUB_ROOT/records" <<'PYSELECTOR'
import json,pathlib,sys
found=False
for p in pathlib.Path(sys.argv[1]).glob('*.stdin'):
    try: data=json.loads(p.read_text())
    except (ValueError,UnicodeError): continue
    if isinstance(data,dict) and data.get('kind')=='List':
        pod=next(o for o in data['items'] if o['kind']=='Pod')
        if pod['spec']['nodeSelector'].get('topology.kubernetes.io/zone')=='example-zone':
            assert pod['spec']['nodeSelector']['kubernetes.io/arch']=='arm64'
            assert not any(v['name']=='sockets' for v in pod['spec']['volumes'])
            assert not any(v['name']=='sockets' for v in pod['spec']['containers'][0]['volumeMounts'])
            found=True
assert found
PYSELECTOR
# A failed new preflight invalidates both old records, including when Service
# resolution fails although socket probes happen to pass.
sed 's/transport: none/transport: service/' "$STUB_ROOT/owner-values.yaml" > "$FERRY_DEPLOY_CONFIG/values.yaml"
export STUB_SERVICE_FAIL=1
if bash "$entry" preflight > "$STUB_ROOT/service-preflight.out" 2>&1; then exit 1; fi
grep -q 'datadog.transport is unworkable' "$STUB_ROOT/service-preflight.out"
[ ! -f "$FERRY_DEPLOY_CONFIG/state/preflight.json" ]
[ ! -f "$FERRY_DEPLOY_CONFIG/state/preflight.yaml" ]
unset STUB_SERVICE_FAIL
cp "$STUB_ROOT/owner-values.yaml" "$FERRY_DEPLOY_CONFIG/values.yaml"
bash "$entry" preflight >/dev/null 2>&1
# A missing default socket directory falls back without creating host paths.
# Its retry budget is independent from the PVC permission retry.
sed '/  transport: none/d' "$STUB_ROOT/owner-values.yaml" > "$FERRY_DEPLOY_CONFIG/values.yaml"
rm "$STUB_ROOT/pvc-attempt"
export STUB_SOCKET_DIR_MISSING=1 STUB_PREFLIGHT_PVC_FAIL_ONCE=1
bash "$entry" preflight > "$STUB_ROOT/socket-fallback.out" 2>&1
grep -q 'socket directory unavailable; probing Service transport without hostPath' "$STUB_ROOT/socket-fallback.out"
grep -q 'transport: service' "$FERRY_DEPLOY_CONFIG/state/preflight.yaml"
grep -q 'fixPermissions: true' "$FERRY_DEPLOY_CONFIG/state/preflight.yaml"
unset STUB_PREFLIGHT_PVC_FAIL_ONCE
# Explicit socket transport remains a hard requirement and cannot silently fall back.
sed 's/transport: none/transport: socket/' "$STUB_ROOT/owner-values.yaml" > "$FERRY_DEPLOY_CONFIG/values.yaml"
if bash "$entry" preflight > "$STUB_ROOT/socket-required.out" 2>&1; then exit 1; fi
grep -q 'FAIL:socket-mount' "$STUB_ROOT/socket-required.out"
if grep -q 'probing Service transport' "$STUB_ROOT/socket-required.out"; then exit 1; fi
[ ! -f "$FERRY_DEPLOY_CONFIG/state/preflight.json" ]
unset STUB_SOCKET_DIR_MISSING
cp "$STUB_ROOT/owner-values.yaml" "$FERRY_DEPLOY_CONFIG/values.yaml"
bash "$entry" preflight >/dev/null 2>&1
# Stale publish must fail before any SSH invocation.
python3 - "$FERRY_DEPLOY_CONFIG/state/last-publish.json" <<'PY'
import json,pathlib,sys
p=pathlib.Path(sys.argv[1]); v=json.loads(p.read_text()); v['commit']='000000000000';p.write_text(json.dumps(v))
PY
ssh_count() {
  python3 - "$STUB_ROOT/records" <<'PYCOUNT'
import json,pathlib,sys
print(sum(json.loads(p.read_text())['tool']=='ssh' for p in pathlib.Path(sys.argv[1]).glob('*.json')))
PYCOUNT
}
before=$(ssh_count)
if bash "$entry" rollout > "$STUB_ROOT/stale.out" 2>&1; then exit 1; fi
grep -q 'run deploy publish first' "$STUB_ROOT/stale.out"
[ "$(ssh_count)" = "$before" ]
python3 - "$STUB_ROOT/records" "$FERRY_DEPLOY_CONFIG" <<'PY'
import json,pathlib,sys
records=[json.loads(p.read_text()) for p in pathlib.Path(sys.argv[1]).glob('*.json')]
# Secret data only travels to apply-secret; bundle stdin contains user values
# and image metadata, and no secrets directory or token file.
import io,tarfile
for p in pathlib.Path(sys.argv[1]).glob('*.stdin'):
    try:
        with tarfile.open(fileobj=io.BytesIO(p.read_bytes())) as tar:
            names=tar.getnames(); assert 'values.yaml' in names
            assert not any('secrets' in name or 'token' in name for name in names)
    except tarfile.ReadError: pass
PY
[ -z "$(find "$STUB_ROOT/remote" -mindepth 1 -print)" ]
printf 'ok: namespace ownership, PVC retry, token rotation, restart marker, bundle and dry-run privacy\n'
