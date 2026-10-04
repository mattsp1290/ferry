#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")/../lib" && pwd)/config.sh"
source "$SKILL_DIR/scripts/lib/values.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/parity.sh"
source "$REPO_ROOT/scripts/lib/extract-configmap.sh"
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
    if printf '%s' "$report" | jq -e -f "$2" >/dev/null; then printf 'ok: %s\n' "$1"; else printf 'FAIL: %s\n' "$1"; failed=1; fi
}
check 'release deployed' "$SKILL_DIR/scripts/jq/release-deployed.jq"
check 'pod running, ready and node matches' "$SKILL_DIR/scripts/jq/pod-ready.jq"
check 'image digest and version' "$SKILL_DIR/scripts/jq/image-version.jq"
if [ "$(printf '%s' "$values" | jq -r '.datadog.transport')" != none ]; then check 'dogstatsd sends in last 15 minutes' "$SKILL_DIR/scripts/jq/recent-dogstatsd.jq"; fi
printf '%s' "$report" | jq -r -f "$SKILL_DIR/scripts/jq/sync-errors.jq" | LC_ALL=C sort | uniq -c | sed 's/^/info: sync failed /'
if ! "$runtime_only"; then
    PARITY_FERRY_BIN=$(ferry_binary)
    PARITY_FORGEJO_URL=$(printf '%s' "$values" | jq -r '.config.forgejo.url')
    umask 077
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-verify.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT
    printf '%s\n' "$values" > "$tmp/values.json"
    helm template "$RELEASE" "$REPO_ROOT/charts/ferry" -f "$tmp/values.json" \
        --show-only templates/configmap.yaml > "$tmp/configmap.yaml"
    PARITY_FERRY_TOML="$tmp/ferry.toml"
    extract_toml "$tmp/configmap.yaml" > "$PARITY_FERRY_TOML"
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
