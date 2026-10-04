#!/usr/bin/env bash
set -euo pipefail
skill=$(cd "$(dirname "$0")/.." && pwd)
python3 - "$skill" <<'PY'
import pathlib,re,sys
root=pathlib.Path(sys.argv[1])
entry=(root/'SKILL.md').read_text()
assert entry.startswith('---\nname: ferry-deploy\n')
assert re.search(r'^description: .+',entry,re.M)
assert len(entry.splitlines()) < 120
for file in [root/'SKILL.md',*root.glob('references/*.md')]:
 for target in re.findall(r'\]\(([^)]+)\)',file.read_text()):
  if '://' not in target:
   assert (file.parent/target.split('#')[0]).exists(), (file,target)
PY
for args in '' nonsense; do
  status=0
  bash "$skill/scripts/deploy.sh" $args >/dev/null 2>&1 || status=$?
  [ "$status" = 2 ]
done
echo 'ok: skill metadata, links and usage'
