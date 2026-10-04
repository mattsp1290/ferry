#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../../../.." && pwd)
umask 077
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-verify-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/cfg/secrets" "$tmp/cfg/state"
export FERRY_DEPLOY_CONFIG="$tmp/cfg" CFG="$tmp/cfg" CONTROL_SSH=control NAMESPACE=ferry RELEASE=ferry KUBECONFIG_PATH=/config HELM_TIMEOUT=5m NODE_SELECTOR=kubernetes.io/arch=amd64 IMAGE_PULL_REPOSITORY=localhost:5000/ferry/ferry
bash "$root/.agents/skills/ferry-deploy/scripts/deploy.sh" init >/dev/null
printf 'obviously-fake-token\n' > "$tmp/cfg/secrets/forgejo-token"
chmod 600 "$tmp/cfg/secrets/forgejo-token"
export PATH="$tmp/bin:$PATH" VERIFY_TEST_ROOT="$tmp" FERRY_BIN=/fake/ferry
printf 'obviously-fake-token\n' > "$CFG/secrets/forgejo-token"
cat > "$tmp/bin/helm" <<'SH'
#!/usr/bin/env bash
case " $* " in
 *' template '*) cat "$VERIFY_TEST_ROOT/values" ;;
 *) exit 1 ;;
esac
SH
cat > "$tmp/bin/ssh" <<'SH'
#!/usr/bin/env bash
jq 'del(.logs)' "$VERIFY_TEST_ROOT/report"
printf '\nFERRY_LOGS\n'
jq -r '.logs' "$VERIFY_TEST_ROOT/report"
SH
chmod +x "$tmp/bin/helm" "$tmp/bin/ssh"
printf '%s\n' '{"config":{"sync":{"poll_interval_seconds":1},"repos":[],"forgejo":{"url":"https://forge.example","username":"ferry"}},"datadog":{"transport":"service"},"nodeSelector":{"kubernetes.io/arch":"amd64","pool":"mirrors"}}' > "$tmp/values"
printf '%s\n' '{"status":{"info":{"status":"deployed"}},"values":{"image":{"digest":"sha256:abc","version":"123456789012"}},"pods":{"items":[{"metadata":{"name":"pod"},"spec":{"nodeName":"node","containers":[{"name":"ferry","image":"localhost:5000/ferry/ferry@sha256:abc","env":[{"name":"DD_VERSION","value":"123456789012"}]}]},"status":{"phase":"Running","conditions":[{"type":"Ready","status":"True"}]}}]},"nodes":{"items":[{"metadata":{"name":"node","labels":{"kubernetes.io/arch":"amd64","pool":"mirrors"}}}]},"logs":"ferry started"}' > "$tmp/good"
jq --slurpfile values "$tmp/values" '.values += $values[0]' "$tmp/good" > "$tmp/new-good"
mv "$tmp/new-good" "$tmp/good"
cp "$tmp/good" "$tmp/report"
bash "$root/.agents/skills/ferry-deploy/scripts/cmd/verify.sh" --runtime-only > "$tmp/output"
for edit in '.status.info.status="failed"' '.pods.items=[]' '.pods.items[0].status.phase="Pending"' '.pods.items[0].status.conditions[0].status="False"' '.nodes.items=[]' '.pods.items[0].spec.containers[0].image="wrong"' '.pods.items[0].spec.containers[0].env[0].value="wrong"' '.nodes.items[0].metadata.labels.pool="other"' '.logs="ferry started dogstatsd send failed"'; do
    jq "$edit" "$tmp/good" > "$tmp/report"
    if bash "$root/.agents/skills/ferry-deploy/scripts/cmd/verify.sh" --runtime-only > "$tmp/output"; then printf 'verify accepted broken check: %s\n' "$edit" >&2; exit 1; fi
done
# A healthy pod remains verifiable after log rotation removes its startup line.
jq '.logs=""' "$tmp/good" > "$tmp/report"
bash "$root/.agents/skills/ferry-deploy/scripts/cmd/verify.sh" --runtime-only > "$tmp/output"
# No local publish state exists: current Helm values remain authoritative.
jq --slurpfile values "$tmp/values" '.values += $values[0]' "$tmp/good" > "$tmp/new-good"
mv "$tmp/new-good" "$tmp/good"
cp "$tmp/good" "$tmp/report"
bash "$root/.agents/skills/ferry-deploy/scripts/cmd/verify.sh" --runtime-only > "$tmp/output"
# Full verification checks parity/topics, and retry starts at invocation.
jq '.config.repos=[{github:"example/source",forgejo:"example/destination"}]' "$tmp/values" > "$tmp/new-values"
mv "$tmp/new-values" "$tmp/values"
jq --slurpfile values "$tmp/values" '.values += $values[0]' "$tmp/good" > "$tmp/report"
cat > "$tmp/bin/git" <<'SH'
#!/usr/bin/env bash
if [ "${3:-}" != ls-remote ]; then exec /usr/bin/git "$@"; fi
case "$4" in
 https://github.com/*) printf 'abcd\trefs/heads/main\n' ;;
 *) n=$(cat "$VERIFY_TEST_ROOT/attempt"); n=$((n + 1)); printf '%s\n' "$n" > "$VERIFY_TEST_ROOT/attempt"
    if [ "$n" -ge 3 ]; then printf 'abcd\trefs/heads/main\n'; else printf 'old\trefs/heads/main\n'; fi ;;
esac
SH
cat > "$tmp/bin/curl" <<'SH'
#!/usr/bin/env bash
cat > "$VERIFY_TEST_ROOT/curl-config"
printf '%s\n' '{"topics":["ferry-mirror"]}'
SH
cat > "$tmp/bin/date" <<'SH'
#!/usr/bin/env bash
if [ "$1" = +%s ]; then cat "$VERIFY_TEST_ROOT/time"; else exec /bin/date "$@"; fi
SH
cat > "$tmp/bin/sleep" <<'SH'
#!/usr/bin/env bash
n=$(cat "$VERIFY_TEST_ROOT/time"); printf '%s\n' "$((n + $1))" > "$VERIFY_TEST_ROOT/time"
SH
chmod +x "$tmp/bin/git" "$tmp/bin/curl" "$tmp/bin/date" "$tmp/bin/sleep"
printf '0\n' > "$tmp/attempt"
printf '100\n' > "$tmp/time"
bash "$root/.agents/skills/ferry-deploy/scripts/cmd/verify.sh" --wait > "$tmp/output" 2> "$tmp/errors" || { cat "$tmp/output" "$tmp/errors"; exit 1; }
[ "$(cat "$tmp/attempt")" = 3 ]
[ "$(cat "$tmp/time")" = 130 ]
grep -q 'ok: mirror topic' "$tmp/output"
cat > "$tmp/bin/curl" <<'SH'
#!/usr/bin/env bash
cat > /dev/null
printf '%s\n' '{"topics":[]}'
SH
if bash "$root/.agents/skills/ferry-deploy/scripts/cmd/verify.sh" > "$tmp/output"; then exit 1; fi
grep -q 'FAIL: mirror topic' "$tmp/output"
# Execute the actual remote snapshot to prove the time-window arguments and
# selector source. A lifetime error must not poison healthy recent logs.
cat > "$tmp/bin/helm" <<'SH'
#!/usr/bin/env bash
case " $* " in
 *' status '*) jq '.status' "$VERIFY_TEST_ROOT/good" ;;
 *' get values '*) jq '.values' "$VERIFY_TEST_ROOT/good" ;;
 *) exit 1 ;;
esac
SH
cat > "$tmp/bin/kubectl" <<'SH'
#!/usr/bin/env bash
case " $* " in
 *' get nodes '*)
    # Verification must use the deployed selector locally, not NODE_SELECTOR.
    case " $* " in *' -l '*) exit 1 ;; esac
    jq '.nodes' "$VERIFY_TEST_ROOT/good" ;;
 *' get pods '*)
    case " $* " in
        *' jsonpath='*) printf 'pod\t2026-01-01T00:00:00Z\n' ;;
        *) jq '.pods' "$VERIFY_TEST_ROOT/good" ;;
    esac ;;
 *' logs '*)
    [ ! -f "$VERIFY_TEST_ROOT/log-read-failed" ] || exit 1
    case " $* " in
        *' --since=15m '*) cat "$VERIFY_TEST_ROOT/recent-logs" ;;
        *) printf 'ferry started\ndogstatsd send failed\n' ;;
    esac ;;
 *) exit 1 ;;
esac
SH
cat > "$tmp/bin/ssh" <<'SH'
#!/usr/bin/env bash
for last; do :; done
bash -c "$last"
SH
chmod +x "$tmp/bin/kubectl"
printf 'recent healthy log without startup\n' > "$tmp/recent-logs"
NODE_SELECTOR=local.setting=stale bash "$root/.agents/skills/ferry-deploy/scripts/cmd/verify.sh" --runtime-only > "$tmp/output"
printf 'dogstatsd send failed\n' > "$tmp/recent-logs"
if bash "$root/.agents/skills/ferry-deploy/scripts/cmd/verify.sh" --runtime-only > "$tmp/output"; then exit 1; fi
grep -q 'FAIL: dogstatsd sends in last 15 minutes' "$tmp/output"
touch "$tmp/log-read-failed"
if bash "$root/.agents/skills/ferry-deploy/scripts/cmd/verify.sh" --runtime-only > "$tmp/output" 2> "$tmp/error"; then exit 1; fi
