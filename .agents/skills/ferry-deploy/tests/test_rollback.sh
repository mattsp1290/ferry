#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../../../.." && pwd)
umask 077
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-rollback-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/home"
export PATH="$tmp/bin:$PATH" HOME="$tmp/home" ROLLBACK_TEST_ROOT="$tmp" KUBECONFIG_PATH=/config RELEASE=ferry NAMESPACE=ferry HELM_TIMEOUT=5m
cat > "$tmp/bin/helm" <<'SH'
#!/usr/bin/env bash
set -eu
case " $* " in
 *' history '*) jq -r '.[] | "\(.revision) Wed Oct 3 10:00:00 2026 \(.status) ferry-0.1.0 0.1.0 description"' "$ROLLBACK_TEST_ROOT/history" ;;
 *' rollback '*) printf '%s\n' "$*" >> "$ROLLBACK_TEST_ROOT/calls" ;;
 *' status '*) printf '%s\n' '{"version":5}' ;;
 *' get values '*) printf '%s\n' '{"image":{"digest":"sha256:abc","version":"123456789012"}}' ;;
 *) exit 1 ;;
esac
SH
chmod +x "$tmp/bin/helm"
printf '%s\n' '[{"revision":1,"status":"deployed"}]' > "$tmp/history"
if bash "$root/.agents/skills/ferry-deploy/scripts/remote/rollback.sh" > "$tmp/output" 2> "$tmp/error"; then exit 1; fi
[ ! -f "$tmp/calls" ]
grep -q 'replicas=0' "$tmp/error"
printf '%s\n' '[{"revision":1,"status":"superseded"},{"revision":2,"status":"deployed"},{"revision":3,"status":"failed"},{"revision":4,"status":"failed"}]' > "$tmp/history"
bash "$root/.agents/skills/ferry-deploy/scripts/remote/rollback.sh" > "$tmp/output"
grep -q 'rollback ferry 1 ' "$tmp/calls"
rm "$tmp/calls"
if REVISION=3 bash "$root/.agents/skills/ferry-deploy/scripts/remote/rollback.sh" > "$tmp/output" 2> "$tmp/error"; then exit 1; fi
[ ! -f "$tmp/calls" ]
REVISION=2 bash "$root/.agents/skills/ferry-deploy/scripts/remote/rollback.sh" > "$tmp/output"
grep -q 'rollback ferry 2 ' "$tmp/calls"
