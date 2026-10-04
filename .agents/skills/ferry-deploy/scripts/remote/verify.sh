#!/usr/bin/env bash
status=$(h status "$RELEASE" -n "$NAMESPACE" -o json)
values=$(h get values "$RELEASE" -n "$NAMESPACE" -a -o json)
pods=$(k get pods -l "app.kubernetes.io/instance=$RELEASE" -o json)
nodes=$(k get nodes -o json)
printf '{"status":%s,"values":%s,"pods":%s,"nodes":%s}\nFERRY_LOGS\n' "$status" "$values" "$pods" "$nodes"
pod_info=$(k get pods -l "app.kubernetes.io/instance=$RELEASE" -o 'jsonpath={range .items[*]}{.metadata.name}{"\t"}{.status.startTime}{"\n"}{end}')
if [ "$(printf '%s\n' "$pod_info" | awk 'NF {count++} END {print count+0}')" = 1 ]; then
    IFS="$(printf '\t')" read -r pod started <<< "$pod_info"
    if [ -n "$started" ]; then
        k logs "$pod" -c ferry --since=15m
    fi
fi
