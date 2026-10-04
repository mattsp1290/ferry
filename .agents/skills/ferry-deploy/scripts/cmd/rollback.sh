#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")/../lib" && pwd)/config.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/state.sh"
revision=''
case "$#" in
    0) ;;
    1) revision=$1 ;;
    2) [ "$1" = --revision ] || exit 2; revision=$2 ;;
    *) exit 2 ;;
esac
case "$revision" in *[!0-9]*) printf 'revision must be an integer\n' >&2; exit 2 ;; esac
snapshot=$(remote_run status)
# Select from structured history on the workstation. A healthy release rolls
# back one successful revision; a failed/pending release recovers its last good
# revision, which Helm can still mark deployed after a failed upgrade.
selected=$(printf '%s' "$snapshot" | jq -r --arg wanted "$revision" '
    .history | sort_by(.revision | tonumber) as $history |
    if $wanted != "" then
      [$history[] | select((.revision|tostring)==$wanted and (.status=="deployed" or .status=="superseded"))]
    elif (($history|last|.status)=="deployed") then
      [$history[] | select(.status=="superseded")]
    else
      [$history[] | select(.status=="deployed" or .status=="superseded")]
    end | last | .revision // empty')
if [ -z "$selected" ]; then
    printf 'No successful revision available for rollback. To stop ferry on the control host:\n' >&2
    printf 'kubectl --kubeconfig %s -n %s scale deployment -l app.kubernetes.io/instance=%s --replicas=0\n' "$KUBECONFIG_PATH" "$NAMESPACE" "$RELEASE" >&2
    exit 1
fi
expected_revision=$(printf '%s' "$snapshot" | jq -r '.history | max_by(.revision | tonumber) | .revision')
expected_status=$(printf '%s' "$snapshot" | jq -r '.history | max_by(.revision | tonumber) | .status')
report=$(remote_run rollback "REVISION=$selected" "EXPECTED_REVISION=$expected_revision" "EXPECTED_STATUS=$expected_status")
result=success
bash "$SKILL_DIR/scripts/cmd/verify.sh" --runtime-only || result=failed
umask 077
version=$(printf '%s' "$report" | jq -r '.values.image.version')
digest=$(printf '%s' "$report" | jq -r '.values.image.digest')
revision=$(printf '%s' "$report" | jq -r '.status.version')
log_deployment rollback "$result" "$revision"
[ "$result" = success ]
