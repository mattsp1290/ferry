#!/usr/bin/env bash
set -euo pipefail
command -v rg >/dev/null || { printf 'test requires rg\n' >&2; exit 1; }
skill=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/repo"
git -C "$work/repo" init -q
python3 - "$work/repo/probe.txt" <<'PY'
import pathlib,sys
pathlib.Path(sys.argv[1]).write_text('private host: '+'.'.join(map(str,[192,168,7,9]))+'\n')
PY
git -C "$work/repo" add probe.txt
cd "$work/repo"
status=0
FERRY_DEPLOY_CONFIG="$work/absent" python3 "$skill/tests/lib/environment_scan.py" > "$work/out" 2>&1 || status=$?
[ "$status" = 1 ]; rg -q 'probe.txt:1' "$work/out"
printf 'https://git.example.com\n' > probe.txt
FERRY_DEPLOY_CONFIG="$work/absent" python3 "$skill/tests/lib/environment_scan.py"
printf '%s%s\n' 'https://' 'private.owner-domain.net' > probe.txt
status=0
FERRY_DEPLOY_CONFIG="$work/absent" python3 "$skill/tests/lib/environment_scan.py" > "$work/out" 2>&1 || status=$?
[ "$status" = 1 ]; rg -q 'probe.txt:1' "$work/out"
echo 'ok: leak scanner catches deliberate private IP and hostname mutations'
