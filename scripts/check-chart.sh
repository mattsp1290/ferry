#!/usr/bin/env bash
# Renders charts/ferry for each datadog.transport value and asserts the
# properties the deployment depends on. Needs helm; uses only POSIX text tools
# for the checks (no yq). When a ferry binary exists, also runs check-config
# against the rendered ferry.toml.
#
#   FERRY_BIN=/path/to/ferry scripts/check-chart.sh
#   FERRY_CHART_STRICT=1 scripts/check-chart.sh   # a skipped check fails
set -euo pipefail

cd "$(dirname "$0")/.."

chart=charts/ferry
digest=sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
common=(--set image.repository=registry.invalid/ferry/ferry --set "image.digest=$digest")

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

failures=0
ok() { echo "ok: $*"; }
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }
# A skipped check is a failure when FERRY_CHART_STRICT=1. CI sets it, so that
# a missing ferry binary cannot quietly drop the check-config assertions.
skip() {
  if [ "${FERRY_CHART_STRICT:-}" = 1 ]; then
    fail "$* (skipped, but FERRY_CHART_STRICT=1)"
  else
    echo "SKIP: $*"
  fi
}

# check <description> <command...>: ok when the command succeeds.
check() {
  local description=$1
  shift
  if "$@" >/dev/null 2>&1; then ok "$description"; else fail "$description"; fi
}

# check_not <description> <command...>: ok when the command fails.
check_not() {
  local description=$1
  shift
  if "$@" >/dev/null 2>&1; then fail "$description"; else ok "$description"; fi
}

# First `path:` value after a probe name in a rendered file.
probe_path() {
  awk -v probe="$2:" '$1 == probe { found = 1 } found && $1 == "path:" { print $2; exit }' "$1"
}

# Body of ferry.toml from the rendered ConfigMap, indentation removed.
extract_toml() {
  awk '
    /^  ferry.toml: \|/ { on = 1; next }
    on && /^    / { sub(/^    /, ""); print; next }
    on && /^$/ { print; next }
    on { exit }
  ' "$1"
}

# Sample allowlist, passed as a values file so that numbers take the same
# path as a real deployment's values file.
cat >"$work/repos.yaml" <<'YAML'
config:
  repos:
    - github: example-owner/example-repo
      forgejo: example-owner/example-repo
      lfs: true
      actions: false
    - github: example-owner/second-repo
      forgejo: mirrors/second-repo
      lfs: false
      adopt: true
YAML

if [ -n "${FERRY_BIN:-}" ]; then
  ferry_bin=$FERRY_BIN
elif [ -x target/debug/ferry ]; then
  ferry_bin=target/debug/ferry
elif [ -x target/release/ferry ]; then
  ferry_bin=target/release/ferry
else
  ferry_bin=
fi

command -v helm >/dev/null || { echo "FAIL: helm is required"; exit 1; }

for transport in socket service none; do
  out="$work/$transport.yaml"
  label="[$transport]"

  if helm lint "$chart" "${common[@]}" --set "datadog.transport=$transport" >"$work/lint.log" 2>&1; then
    ok "$label helm lint"
  else
    fail "$label helm lint"
    cat "$work/lint.log"
  fi

  if ! helm template ferry "$chart" "${common[@]}" --set "datadog.transport=$transport" \
    -f "$work/repos.yaml" >"$out" 2>"$work/template.err"; then
    fail "$label helm template"
    cat "$work/template.err"
    continue
  fi
  ok "$label helm template"

  check "$label one replica" grep -qx '  replicas: 1' "$out"
  check "$label strategy Recreate" grep -qx '    type: Recreate' "$out"
  check "$label image pinned by digest" grep -q "image: \"registry.invalid/ferry/ferry@sha256:" "$out"
  check_not "$label no Service object" grep -qE '^kind: (Service|Ingress|NetworkPolicy|Role|RoleBinding|ClusterRole|ClusterRoleBinding)$' "$out"
  check "$label Secret defaultMode 0440 (288)" grep -qE '^ +defaultMode: (288|0440)$' "$out"
  check "$label startup probe on /healthz" test "$(probe_path "$out" startupProbe)" = /healthz
  check "$label liveness probe on /healthz" test "$(probe_path "$out" livenessProbe)" = /healthz
  check "$label readiness probe on /readyz" test "$(probe_path "$out" readinessProbe)" = /readyz
  check "$label main container runs as non-root" grep -q 'runAsNonRoot: true' "$out"
  check "$label read-only root filesystem" grep -q 'readOnlyRootFilesystem: true' "$out"
  check "$label drops all capabilities" grep -qE '^ +drop: \["ALL"\]$' "$out"
  check_not "$label no privilege escalation" grep -q 'allowPrivilegeEscalation: true' "$out"
  check "$label automountServiceAccountToken false" grep -q 'automountServiceAccountToken: false' "$out"
  check "$label terminationGracePeriodSeconds 40" grep -q 'terminationGracePeriodSeconds: 40' "$out"
  check "$label checksum/config annotation" grep -qE 'checksum/config: "?[0-9a-f]{64}"?$' "$out"
  check "$label datadog logs annotation" grep -q 'ad.datadoghq.com/ferry.logs:' "$out"
  check_not "$label no fix-permissions initContainer by default" grep -q 'initContainers:' "$out"

  case $transport in
    socket)
      check "$label hostPath mount present" grep -q 'hostPath:' "$out"
      check "$label hostPath is /var/run/datadog" grep -q 'path: "/var/run/datadog"' "$out"
      check "$label DD_DOGSTATSD_URL is the unix socket" grep -q 'value: unix:///var/run/datadog/dsd.socket' "$out"
      check "$label DD_TRACE_AGENT_URL is the unix socket" grep -q 'value: unix:///var/run/datadog/apm.socket' "$out"
      ;;
    service)
      check_not "$label no hostPath" grep -q 'hostPath:' "$out"
      check "$label DD_DOGSTATSD_URL targets the agent service" grep -q 'value: "udp://datadog-agent.default.svc.cluster.local:8125"' "$out"
      check "$label DD_TRACE_AGENT_URL targets the agent service" grep -q 'value: "http://datadog-agent.default.svc.cluster.local:8126"' "$out"
      ;;
    none)
      check_not "$label no hostPath" grep -q 'hostPath:' "$out"
      check_not "$label no DD_DOGSTATSD_URL" grep -q 'DD_DOGSTATSD_URL' "$out"
      check_not "$label no DD_TRACE_AGENT_URL" grep -q 'DD_TRACE_AGENT_URL' "$out"
      ;;
  esac

  if [ -n "$ferry_bin" ]; then
    extract_toml "$out" >"$work/$transport.toml"
    if "$ferry_bin" check-config --config "$work/$transport.toml" >"$work/check.log" 2>&1; then
      ok "$label rendered ferry.toml passes check-config"
    else
      fail "$label rendered ferry.toml passes check-config"
      cat "$work/check.log"
      cat "$work/$transport.toml"
    fi
  else
    skip "$label check-config (no ferry binary; set FERRY_BIN or run cargo build)"
  fi
done

# fixPermissions renders the initContainer.
fix="$work/fix.yaml"
if helm template ferry "$chart" "${common[@]}" --set cache.fixPermissions=true >"$fix" 2>&1; then
  check "fixPermissions renders the initContainer" grep -q 'name: fix-permissions' "$fix"
  check "fixPermissions initContainer runs as root" grep -q 'runAsUser: 0' "$fix"
  check "fixPermissions initContainer adds only CHOWN" grep -qE '^ +add: \["CHOWN"\]$' "$fix"
  check "fixPermissions initContainer keeps a read-only root" test "$(grep -c 'readOnlyRootFilesystem: true' "$fix")" = 2
else
  fail "fixPermissions render"
  cat "$fix"
fi

# Integer overrides must stay integers in ferry.toml.
if [ -n "$ferry_bin" ]; then
  int="$work/int.yaml"
  if helm template ferry "$chart" "${common[@]}" --set config.sync.poll_interval_seconds=600 \
    --set config.sync.max_concurrency=4 -s templates/configmap.yaml >"$int" 2>&1; then
    extract_toml "$int" >"$work/int.toml"
    check "integer --set override renders as an integer" grep -qx 'poll_interval_seconds = 600' "$work/int.toml"
    check "integer --set override passes check-config" "$ferry_bin" check-config --config "$work/int.toml"
  else
    fail "integer --set override render"
    cat "$int"
  fi
else
  skip "integer --set override (no ferry binary)"
fi

# check_render_fails <value-name> <helm args...>: the render must fail, and
# the error must name the value so the operator knows what to set.
check_render_fails() {
  local value=$1
  shift
  local out="$work/render-fails.out"
  if helm template ferry "$chart" "$@" >"$out" 2>&1; then
    fail "bad $value fails the render"
  elif grep -q "$value" "$out"; then
    ok "bad $value fails the render and names $value"
  else
    fail "bad $value error does not name $value"
  fi
}

check_render_fails image.digest --set image.repository=x
check_render_fails image.repository --set image.digest=sha256:0
check_render_fails datadog.transport "${common[@]}" --set datadog.transport=carrier-pigeon

if [ "$failures" -ne 0 ]; then
  echo "$failures assertion(s) failed"
  exit 1
fi
echo "all chart checks passed"
