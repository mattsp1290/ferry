#!/usr/bin/env bash
# Helm performs YAML parsing and merging; jq reads only rendered JSON.
values_render() {
    helm template values-reader "$SKILL_DIR/resources/values-reader" "$@" | sed '/^---$/d; /^# Source:/d' | jq -ce 'select(type == "object")'
}
merged_values() {
    if [ -f "$CFG/state/preflight.yaml" ]; then
        values_render -f "$REPO_ROOT/charts/ferry/values.yaml" -f "$CFG/state/preflight.yaml" -f "$CFG/values.yaml"
    else
        values_render -f "$REPO_ROOT/charts/ferry/values.yaml" -f "$CFG/values.yaml"
    fi
}
owner_values_json() { values_render -f "$CFG/values.yaml"; }
