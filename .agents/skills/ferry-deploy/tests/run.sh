#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../../.."
failed=0
for test in .agents/skills/ferry-deploy/tests/test_*.sh; do
  if bash "$test"; then
    printf 'PASS %s\n' "${test##*/}"
  else
    printf 'FAIL %s\n' "${test##*/}" >&2
    failed=1
  fi
done
while IFS= read -r script; do bash -n "$script" || failed=1; done < <(find .agents/skills/ferry-deploy -name '*.sh' -type f)
if command -v shellcheck >/dev/null; then
  while IFS= read -r script; do shellcheck -x "$script" || failed=1; done < <(find .agents/skills/ferry-deploy/scripts -name '*.sh' -type f)
else
  echo 'SKIP shellcheck: not installed'
fi
exit "$failed"
