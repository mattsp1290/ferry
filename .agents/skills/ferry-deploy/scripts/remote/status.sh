#!/usr/bin/env bash
status=$(h status "$RELEASE" -n "$NAMESPACE" -o json)
values=$(h get values "$RELEASE" -n "$NAMESPACE" -a -o json)
pods=$(k get pods -l "app.kubernetes.io/instance=$RELEASE" -o json)
history=$(h history "$RELEASE" -n "$NAMESPACE" -o json)
printf '{"status":%s,"values":%s,"pods":%s,"history":%s}\n' "$status" "$values" "$pods" "$history"
