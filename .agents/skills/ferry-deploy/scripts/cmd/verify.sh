#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")/../lib" && pwd)/config.sh"
source "$SKILL_DIR/scripts/lib/values.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/parity.sh"
invoked=$(date +%s)
wait=false
only_repo=''
runtime_only=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --wait) wait=true; shift ;;
        --repo) [ "$#" -ge 2 ] || exit 2; only_repo=$2; shift 2 ;;
        --runtime-only) runtime_only=true; shift ;;
        *) printf 'unknown verify option\n' >&2; exit 2 ;;
    esac
done
snapshot=$(remote_run verify)
report=$(printf '%s' "$snapshot" | jq -Rs 'split("\nFERRY_LOGS") | (.[0]|fromjson) + {logs:(.[1:]|join("\nFERRY_LOGS"))}')
values=$(printf '%s' "$report" | jq -c '.values')
failed=0
check() {
    if printf '%s' "$report" | jq -e "$2" >/dev/null; then printf 'ok: %s\n' "$1"; else printf 'FAIL: %s\n' "$1"; failed=1; fi
}
check 'release deployed' '.status.info.status == "deployed"'
check 'pod running, ready and node matches' '. as $r | ($r.pods.items | length == 1) and ($r.pods.items[0] | .status.phase == "Running" and any(.status.conditions[]?; .type == "Ready" and .status == "True")) and any($r.nodes.items[]?; .metadata.name == $r.pods.items[0].spec.nodeName)'
check 'image digest and version' '. as $r | ($r.values.image.digest | type == "string" and startswith("sha256:")) and ($r.values.image.version | type == "string" and length > 0) and any($r.pods.items[0].spec.containers[]?; .name == "ferry" and (.image | endswith("@" + $r.values.image.digest)) and any(.env[]?; .name == "DD_VERSION" and .value == $r.values.image.version))'
check 'startup logged' '.logs | contains("ferry started")'
if [ "$(printf '%s' "$values" | jq -r '.datadog.transport')" != none ]; then check 'dogstatsd sends' '.logs | contains("dogstatsd send failed") | not'; fi
printf '%s' "$report" | jq -r '.logs | split("\n")[] | fromjson? | select(.message == "sync failed" or .fields.message == "sync failed") | [(.repo // .fields.repo // "unknown"), (.error_kind // .fields.error_kind // "unknown")] | @tsv' | LC_ALL=C sort | uniq -c | sed 's/^/info: sync failed /'
if ! "$runtime_only"; then
    PARITY_FERRY_BIN=$(ferry_binary)
    PARITY_FORGEJO_URL=$(printf '%s' "$values" | jq -r '.config.forgejo.url')
    PARITY_FORGEJO_HOST=$(printf '%s' "$PARITY_FORGEJO_URL" | sed -E 's@^[a-z]+://([^/]+).*@\1@')
    PARITY_FORGEJO_USER=$(printf '%s' "$values" | jq -r '.config.forgejo.username')
    deadline=$((invoked + $(printf '%s' "$values" | jq -r '.config.sync.poll_interval_seconds') + 60))
    repos=$(printf '%s' "$values" | jq -r --arg repo "$only_repo" '.config.repos[] | select($repo == "" or .github == $repo) | [.github,.forgejo] | @tsv')
    if [ -z "$repos" ]; then printf 'FAIL: no matching allowlist entries\n'; failed=1; fi
    while IFS="$(printf '\t')" read -r github forgejo; do
        [ -n "$github" ] || continue
        parity_ok=false
        while true; do
            if ref_parity "$github" "$forgejo"; then parity_ok=true; break; fi
            if ! "$wait" || [ "$(date +%s)" -ge "$deadline" ]; then failed=1; break; fi
            remaining=$((deadline - $(date +%s)))
            [ "$remaining" -gt 0 ] || { failed=1; break; }
            delay=15
            [ "$remaining" -ge "$delay" ] || delay=$remaining
            sleep "$delay"
        done
        if "$parity_ok"; then printf 'ok: mirror parity %s\n' "$github"; else failed=1; printf 'FAIL: mirror parity %s\n' "$github"; fi
        if repo_has_mirror_topic "$forgejo"; then printf 'ok: mirror topic %s\n' "$github"; else failed=1; printf 'FAIL: mirror topic %s\n' "$github"; fi
    done <<< "$repos"
fi
exit "$failed"
