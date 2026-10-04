#!/usr/bin/env bash
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-deploy.XXXXXX")
name=$POD_NAME
cleanup() {
  k delete pod,pvc -l "ferry-deploy-preflight=$name" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
cat > "$tmp/apply.json"
k apply -f "$tmp/apply.json" >/dev/null
end=$((SECONDS + 300))
while :; do
  reasons=$(k get pod "$name" -o 'jsonpath={range .status.containerStatuses[*]}{.state.waiting.reason}{"\n"}{end}{range .status.initContainerStatuses[*]}{.state.waiting.reason}{"\n"}{end}')
  case "$reasons" in *ErrImagePull*|*ImagePullBackOff*) echo 'digest is not pullable through IMAGE_PULL_REPOSITORY on the selected node' >&2; exit 1;; esac
  phase=$(k get pod "$name" -o 'jsonpath={.status.phase}')
  case "$phase" in Succeeded|Failed) break;; esac
  mount_errors=$(k get events --field-selector "involvedObject.name=$name,reason=FailedMount" -o 'jsonpath={range .items[*]}{.message}{"\n"}{end}')
  case "$mount_errors" in
    *'volume "datadog-socket"'*'hostPath type check failed:'*'is not a directory'*)
      printf 'FAIL:socket-mount\n'
      exit 4;;
  esac
  [ "$SECONDS" -lt "$end" ] || { echo 'preflight pod timed out' >&2; exit 1; }
  sleep 2
done
k logs "$name"
printf 'phase:%s\n' "$phase"
