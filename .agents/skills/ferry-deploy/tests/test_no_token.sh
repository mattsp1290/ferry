#!/usr/bin/env bash
set -euo pipefail
skill=$(cd "$(dirname "$0")/.." && pwd)
source "$skill/tests/lib/stubs.sh"
stubs_init
trap 'rm -rf "$STUB_ROOT"' EXIT
for command in check publish preflight rollout verify; do
  if [ "$command" = check ]; then
    bash "$skill/scripts/deploy.sh" check --forgejo > "$STUB_ROOT/$command.out" 2>&1
  else
    bash "$skill/scripts/deploy.sh" "$command" > "$STUB_ROOT/$command.out" 2>&1
  fi
done
python3 - "$STUB_ROOT" "$skill" <<'PY'
import base64,json,os,pathlib,re,subprocess,sys
root=pathlib.Path(sys.argv[1]); skill=pathlib.Path(sys.argv[2])
token=(root/'config/secrets/forgejo-token').read_bytes().rstrip(); needles=[token,base64.b64encode(token)]
def leaked():
    failures=[]
    for path in root.rglob('*'):
        if not path.is_file() or path==root/'config/secrets/forgejo-token' or path.suffix=='.stdin': continue
        data=path.read_bytes()
        if any(needle in data for needle in needles): failures.append(str(path))
    return failures
assert not leaked(), 'Token leakage: '+str(leaked())
assert not list((root/'remote').iterdir()), 'Remote temporary files remained'
records=[json.loads(path.read_text()) for path in (root/'records').glob('*.json')]
assert any(r['tool']=='ssh' and 'bash' in r['argv'][-1] for r in records), 'SSH stub did not execute remote scripts'
assert any(r['tool']=='git' and 'ls-remote' in r['argv'] for r in records), 'Verification did not check parity'
assert any(r['tool']=='build' for r in records) and any(r['tool']=='assemble' for r in records), 'Publish did not build and assemble'
assert any(r['tool']=='curl' for r in records), 'Token checks did not invoke curl'
# Demonstrate both prohibited channels fail the same scanner, then restore.
before=set((root/'records').glob('*'))
subprocess.run(['jq','-n','--arg','leaked',token.decode(),'$leaked'],stdout=subprocess.DEVNULL,check=True)
assert leaked(), 'Scanner missed deliberate jq --arg token leak'
for path in set((root/'records').glob('*'))-before: path.unlink()
environment=dict(os.environ,DELIBERATE_TOKEN_LEAK=token.decode())
subprocess.run(['docker','info'],env=environment,check=True)
assert leaked(), 'Scanner missed deliberate child environment token leak'
for path in set((root/'records').glob('*'))-before: path.unlink()
assert not leaked()
for path in (skill/'scripts').rglob('*.sh'):
    for expression in re.finditer(r'\$\([^)]*\)|<<<[^\n]*',path.read_text()):
        if re.search(r'(?:secrets/|state/gitops/)',expression.group()):
            raise AssertionError('Token file command substitution/here-string: '+str(path))
print('ok: full deployment sequence keeps tokens off argv, environment, output and disk; argv/env mutations detected')
PY
