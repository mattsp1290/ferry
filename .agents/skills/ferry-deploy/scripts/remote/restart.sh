#!/usr/bin/env bash
release_lock

names=$(k get deployment -l "app.kubernetes.io/instance=$RELEASE" -o 'jsonpath={range .items[*]}{.metadata.name}{"\n"}{end}')
[ "$(printf '%s\n' "$names" | awk 'NF {count++} END {print count+0}')" = 1 ] || { printf 'expected exactly one release Deployment\n' >&2; exit 1; }
name=$names
k rollout restart "deployment/$name"
k rollout status "deployment/$name" --timeout="$HELM_TIMEOUT"
