#!/usr/bin/env bash
set -euo pipefail
skill=$(cd "$(dirname "$0")/.." && pwd)
umask 077
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-rollback-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/home" "$tmp/repo/.agents/skills"
cp -R "$skill" "$tmp/repo/.agents/skills/ferry-deploy"
# Isolate selection and locked Helm execution; runtime verification has its
# own scenario suite and is only asserted to run after a successful rollback.
printf '#!/usr/bin/env bash\ntouch "$ROLLBACK_TEST_ROOT/verified"\n' > "$tmp/repo/.agents/skills/ferry-deploy/scripts/cmd/verify.sh"
export HOME="$tmp/home" FERRY_DEPLOY_CONFIG="$tmp/config" ROLLBACK_TEST_ROOT="$tmp"
entry="$tmp/repo/.agents/skills/ferry-deploy/scripts/deploy.sh"
bash "$entry" init >/dev/null
printf 'obviously-fake-token\n' > "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"
export PATH="$tmp/bin:$PATH"
cat > "$tmp/bin/ssh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
bash -c "${!#}"
SH
cat > "$tmp/bin/kubectl" <<'SH'
#!/usr/bin/env bash
printf '%s\n' '{"items":[]}'
SH
cat > "$tmp/bin/helm" <<'SH'
#!/usr/bin/env bash
set -eu
case " $* " in
 *' history '*)
   case " $* " in
     *' -o json '*) cat "$ROLLBACK_TEST_ROOT/history" ;;
     *)
       if [ "${STUB_ROLLBACK_STALE:-}" = 1 ]; then printf '5 Sun Oct 4 10:00:00 2026 pending-upgrade ferry-0.1.0 0.1.0 description\n';
       else jq -r '.[] | "\(.revision) Sun Oct 4 10:00:00 2026 \(.status) ferry-0.1.0 0.1.0 description"' "$ROLLBACK_TEST_ROOT/history"; fi ;;
   esac ;;
 *' rollback '*) printf '%s\n' "$*" >> "$ROLLBACK_TEST_ROOT/calls" ;;
 *' status '*) printf '%s\n' '{"version":5,"info":{"status":"deployed"}}' ;;
 *' get values '*) printf '%s\n' '{"image":{"digest":"sha256:abc","version":"123456789012"}}' ;;
 *) exit 1 ;;
esac
SH
chmod +x "$tmp/bin/"*
run_case() {
    local history=$1 expected=$2
    shift 2
    printf '%s\n' "$history" > "$tmp/history"
    rm -f "$tmp/calls" "$tmp/verified"
    if [ -n "$expected" ]; then
        bash "$entry" rollback "$@" > "$tmp/output" 2> "$tmp/error"
        grep -q "rollback ferry $expected " "$tmp/calls"
        [ -f "$tmp/verified" ]
    else
        if bash "$entry" rollback "$@" > "$tmp/output" 2> "$tmp/error"; then exit 1; fi
        [ ! -f "$tmp/calls" ]
        [ ! -f "$tmp/verified" ]
        grep -q 'replicas=0' "$tmp/error"
    fi
}
run_case '[{"revision":1,"status":"deployed"}]' ''
run_case '[{"revision":1,"status":"deployed"},{"revision":2,"status":"failed"}]' 1
run_case '[{"revision":1,"status":"superseded"},{"revision":2,"status":"deployed"}]' 1
history='[{"revision":1,"status":"superseded"},{"revision":2,"status":"deployed"},{"revision":3,"status":"failed"},{"revision":4,"status":"failed"}]'
run_case "$history" 2
run_case "$history" '' 3
run_case "$history" 2 2
run_case '[{"revision":1,"status":"superseded"},{"revision":2,"status":"deployed"},{"revision":3,"status":"pending-upgrade"}]' 2
run_case '[{"revision":1,"status":"failed"},{"revision":2,"status":"pending-install"}]' ''
printf '%s\n' "$history" > "$tmp/history"
rm -f "$tmp/calls" "$tmp/verified"
export STUB_ROLLBACK_STALE=1
if bash "$entry" rollback > "$tmp/output" 2> "$tmp/error"; then exit 1; fi
[ ! -f "$tmp/calls" ]
[ ! -f "$tmp/verified" ]
grep -q 'release history changed before rollback' "$tmp/error"
unset STUB_ROLLBACK_STALE
printf 'ok: rollback healthy revision, failed/pending recovery and invalid target refusal\n'
