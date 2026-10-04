#!/usr/bin/env bash
set -euo pipefail
umask 077
skill=$(cd "$(dirname "$0")/.." && pwd)
source "$skill/tests/lib/stubs.sh"
stubs_init
trap 'rm -rf "$STUB_ROOT"' EXIT
entry="$skill/scripts/deploy.sh"
bash "$entry" publish >/dev/null
export STUB_NAMESPACE_UNOWNED=1
if bash "$entry" preflight > "$STUB_ROOT/unowned.out" 2>&1; then echo 'unowned namespace accepted' >&2; exit 1; fi
python3 - "$STUB_ROOT/records" <<'PY'
import json,pathlib,sys
assert not any('apply' in json.loads(p.read_text())['argv'] for p in pathlib.Path(sys.argv[1]).glob('*.json'))
PY
unset STUB_NAMESPACE_UNOWNED
export STUB_PREFLIGHT_PVC_FAIL_ONCE=1
bash "$entry" preflight >/dev/null 2>&1
rg -q 'fixPermissions: true' "$FERRY_DEPLOY_CONFIG/state/preflight.yaml"
[ -f "$FERRY_DEPLOY_CONFIG/state/secret-restart-pending" ]
unset STUB_PREFLIGHT_PVC_FAIL_ONCE
export STUB_KEEP_UID=1 STUB_DRY_SECRET=DO-NOT-ECHO-RENDERED-SECRET
bash "$entry" rollout > "$STUB_ROOT/rollout.out" 2>&1
! rg -q DO-NOT-ECHO-RENDERED-SECRET "$STUB_ROOT/rollout.out"
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
rg -qx changed "$STUB_ROOT/rotation-preflight.out"
rotation_version=$(cat "$STUB_ROOT/secret-present")
unset STUB_SECRET_CHANGED
bash "$entry" rollout > "$STUB_ROOT/rotation-rollout.out" 2>&1
rg -qx unchanged "$STUB_ROOT/rotation-rollout.out"
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
if bash "$entry" rollout >/dev/null 2>&1; then echo 'failed upgrade accepted' >&2; exit 1; fi
[ -f "$FERRY_DEPLOY_CONFIG/state/secret-restart-pending" ]
unset STUB_SECRET_CHANGED STUB_UPGRADE_FAIL
bash "$entry" rollout >/dev/null 2>&1
[ "$(restart_count)" = 3 ]
# All-digit commit IDs stay strings in both serialized values and Helm output.
bash "$entry" rollout --digest "$STUB_DIGEST" --version 123456789012 >/dev/null 2>&1
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
rg -q 'run deploy publish first' "$STUB_ROOT/stale.out"
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
