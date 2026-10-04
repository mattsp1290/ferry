#!/usr/bin/env bash
printf '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"%s","labels":{"app.kubernetes.io/managed-by":"ferry-deploy"}}}\n' "$NAMESPACE" | k apply -f - >/dev/null
