#!/usr/bin/env bash
before=$(k get secret "$SECRET_NAME" --ignore-not-found -o 'jsonpath={.metadata.resourceVersion}')
k apply --server-side --field-manager=ferry-deploy -f - >/dev/null
after=$(k get secret "$SECRET_NAME" -o 'jsonpath={.metadata.resourceVersion}')
if [ -z "$before" ]; then printf 'created\n'; elif [ "$before" = "$after" ]; then printf 'unchanged\n'; else printf 'changed\n'; fi
