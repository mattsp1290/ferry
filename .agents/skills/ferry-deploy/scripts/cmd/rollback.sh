#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")/../lib" && pwd)/config.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
revision=''
case "$#" in
    0) ;;
    1) revision=$1 ;;
    2) [ "$1" = --revision ] || exit 2; revision=$2 ;;
    *) exit 2 ;;
esac
case "$revision" in *[!0-9]*) printf 'revision must be an integer\n' >&2; exit 2 ;; esac
report=$(remote_run rollback "REVISION=$revision")
result=success
bash "$SKILL_DIR/scripts/cmd/verify.sh" --runtime-only || result=failed
umask 077
printf '%s' "$report" | jq -c --arg time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg result "$result" '{revision:.status.version,commit:.values.image.version,digest:.values.image.digest,time:$time,action:"rollback",result:$result}' >> "$CFG/state/deployments.jsonl"
[ "$result" = success ]
