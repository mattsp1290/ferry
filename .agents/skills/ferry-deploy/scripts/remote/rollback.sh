#!/usr/bin/env bash
case "${REVISION:-}" in ''|*[!0-9]*) printf 'rollback requires a selected successful revision\n' >&2; exit 2;; esac
release_lock

# Selection used structured JSON on the workstation; recheck the history head
# while holding the release lock so another deploy cannot make it stale.
head=$(h history "$RELEASE" -n "$NAMESPACE" | awk '
  $1 ~ /^[0-9]+$/ {
    for (i=2;i<=NF;i++) {
      if ($i=="deployed" || $i=="superseded" || $i=="failed" || $i ~ /^pending-/ || $i=="uninstalled" || $i=="uninstalling" || $i=="unknown") {
        if ($1+0 > revision+0) {revision=$1; status=$i}; break
      }
    }
  }
  END {if (revision != "") print revision ":" status}
')
if [ "$head" != "$EXPECTED_REVISION:$EXPECTED_STATUS" ]; then
    printf 'release history changed before rollback; inspect deploy status before retrying\n' >&2
    exit 1
fi
h rollback "$RELEASE" "$REVISION" -n "$NAMESPACE" --wait --timeout "$HELM_TIMEOUT" >&2
status=$(h status "$RELEASE" -n "$NAMESPACE" -o json)
values=$(h get values "$RELEASE" -n "$NAMESPACE" -a -o json)
printf '{"status":%s,"values":%s}\n' "$status" "$values"
