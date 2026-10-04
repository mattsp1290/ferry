#!/usr/bin/env bash
set -euo pipefail
skill=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/repo/.agents/skills" "$work/repo/docker" "$work/bin" "$work/home"
cp -R "$skill" "$work/repo/.agents/skills/ferry-deploy"
printf 'FROM scratch\n' > "$work/repo/docker/base.Dockerfile"
git -C "$work/repo" init -q
git -C "$work/repo" add .
git -C "$work/repo" -c user.name=Test -c user.email=test@example.com commit -qm fixture
export HOME="$work/home" FERRY_DEPLOY_CONFIG="$work/config" TRACE="$work/trace" MODE=reuse
entry="$work/repo/.agents/skills/ferry-deploy/scripts/deploy.sh"
bash "$entry" init >/dev/null
printf 'obviously-fake-test-token\n' > "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"
chmod 600 "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"
cat > "$work/bin/crane" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TRACE"
[ "$1" != --insecure ] || shift
case $1 in
 digest)
  case "$2" in */base:*) [ "${BASE_MISSING:-0}" != 1 ] || [ -f "$TRACE.base" ] || exit 1; printf 'sha256:%064d\n' 1 ;;
   *) [ "$MODE" = reuse ] || [ -e "$TRACE.built" ] || exit 1; printf 'sha256:%064d\n' 2 ;; esac ;;
 push) touch "$TRACE.base" ;;
 config) echo '{"os":"linux","architecture":"amd64"}' ;;
 manifest) echo '{"schemaVersion":2,"layers":[]}' ;;
 *) exit 1 ;;
esac
STUB
cat > "$work/bin/build" <<'STUB'
#!/usr/bin/env bash
printf 'build\n' >> "$TRACE"
STUB
cat > "$work/bin/assemble" <<'STUB'
#!/usr/bin/env bash
printf 'assemble\n' >> "$TRACE"
touch "$TRACE.built"
if [ "$MODE" = mismatch ]; then printf 'sha256:%064d\n' 3; else printf 'sha256:%064d\n' 2; fi
STUB
cat > "$work/bin/docker" <<'STUB'
#!/usr/bin/env bash
printf 'docker %s\n' "$*" >> "$TRACE"
[ "${BASE_MISSING:-0}" = 1 ] || exit 1
if [ "$1" = save ]; then
  while [ "$#" -gt 0 ]; do
    if [ "$1" = -o ]; then shift; : > "$1"; break; fi
    shift
  done
fi
STUB
chmod +x "$work/bin/"*
export PATH="$work/bin:$PATH" FERRY_DEPLOY_BUILD_CMD="$work/bin/build" FERRY_DEPLOY_ASSEMBLE_CMD="$work/bin/assemble"
bash "$entry" publish > "$work/out"
! rg -q '^build|^assemble|^docker' "$TRACE"
rg -q '^sha256:' "$work/out"
MODE=build; export MODE
bash "$entry" publish >/dev/null
rg -q '^build' "$TRACE"
! rg -q '^docker' "$TRACE"
rm "$TRACE.built"
MODE=mismatch; export MODE
status=0; bash "$entry" publish > "$work/out" 2>&1 || status=$?
[ "$status" = 1 ]; rg -q 'digest mismatch' "$work/out"
sed 's/^REGISTRY_INSECURE=.*/REGISTRY_INSECURE=true/' "$FERRY_DEPLOY_CONFIG/deploy.env" > "$work/env"
cat "$work/env" > "$FERRY_DEPLOY_CONFIG/deploy.env"
MODE=reuse; export MODE
bash "$entry" publish >/dev/null
rg -q '^--insecure digest' "$TRACE"
rm -f "$TRACE.built"
export BASE_MISSING=1 MODE=build TMPDIR="$work/temporary"
mkdir "$TMPDIR"
bash "$entry" publish >/dev/null
rg -q '^docker build --platform linux/amd64 --provenance=false' "$TRACE"
rg -q '^docker save --platform linux/amd64' "$TRACE"
rg -q '^docker image rm' "$TRACE"
[ -z "$(ls -A "$TMPDIR")" ]
unset BASE_MISSING
touch "$work/repo/dirty"
status=0; bash "$entry" publish > "$work/out" 2>&1 || status=$?
[ "$status" = 2 ]; rg -q 'clean work tree' "$work/out"
echo 'ok: publish reuse, insecure policy, mismatch and dirty-tree refusal'
