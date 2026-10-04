#!/usr/bin/env bash
set -euo pipefail
SKILL_DIR=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
umask 077
CFG=$work/config
mkdir -p "$CFG/secrets" "$CFG/state" "$work/bin" "$work/remote"
printf 'obviously-fake-forgejo  \t\n\n' > "$CFG/secrets/forgejo-token"
printf 'obviously-fake-github\342\200\203\n' > "$CFG/secrets/github-token"
NAMESPACE=ferry RELEASE=ferry KUBECONFIG_PATH=/tmp/kube.conf HELM_TIMEOUT=5m NODE_SELECTOR=kubernetes.io/arch=amd64 IMAGE_PULL_REPOSITORY=localhost:5000/ferry/ferry CONTROL_SSH=admin@control.example.internal
export SKILL_DIR CFG NAMESPACE RELEASE KUBECONFIG_PATH HELM_TIMEOUT NODE_SELECTOR IMAGE_PULL_REPOSITORY CONTROL_SSH
config_error() { printf '%s\n' "$*" >&2; exit 2; }
with_token_fd() { local p=$1 fd=$2; shift 2; [ "$fd" = 8 ]; "$@" 8< "$p"; }
merged_values() { printf '{"credentialsSecret":"ferry-credentials"}\n'; }
source "$SKILL_DIR/scripts/lib/remote.sh"
source "$SKILL_DIR/scripts/lib/secret.sh"
secret_manifest > "$work/manifest"
jq -e '.data["forgejo-token"]==("obviously-fake-forgejo"|@base64) and .data["github-token"]==("obviously-fake-github"|@base64)' "$work/manifest" >/dev/null
rm "$CFG/secrets/github-token"
secret_manifest | jq -e '.data|has("github-token")|not' >/dev/null
cat > "$work/bin/ssh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
shift 4
printf '%s\n' "$*" >> "$RECORD/argv"
bash -c "$*"
SH
cat > "$work/bin/kubectl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$RECORD/kubectl"
case " $* " in
  *' apply '*) cat > "$RECORD/stdin"; touch "$RECORD/applied";;
  *' get secret '*)
    if [ -f "$RECORD/applied" ]; then printf '%s' "$AFTER"; else printf '%s' "$BEFORE"; fi;;
esac
SH
chmod +x "$work/bin/ssh" "$work/bin/kubectl"
export PATH="$work/bin:$PATH" RECORD="$work/remote"
for scenario in created changed unchanged; do
  rm -f "$RECORD/applied" "$CFG/state/secret-restart-pending"
  case "$scenario" in created) BEFORE= AFTER=2;; changed) BEFORE=1 AFTER=2;; unchanged) BEFORE=2 AFTER=2;; esac
  export BEFORE AFTER
  result=$(apply_secret)
  [ "$result" = "$scenario" ]
  if [ "$scenario" = unchanged ]; then [ ! -f "$CFG/state/secret-restart-pending" ]; else [ -f "$CFG/state/secret-restart-pending" ]; fi
  jq -e '.kind=="Secret" and .data["forgejo-token"]==("obviously-fake-forgejo"|@base64)' "$RECORD/stdin" >/dev/null
done
if grep -E 'obviously-fake|b2J2aW91c2x5LWZha2U' "$RECORD/argv" "$RECORD/kubectl"; then exit 1; fi
# The transported program decodes byte for byte to the checked-in source.
python3 - "$RECORD/argv" "$SKILL_DIR/scripts/remote/apply-secret.sh" <<'PY'
import base64,re,sys
line=open(sys.argv[1]).readline()
match=re.search(r'printf %s ([A-Za-z0-9+/=]+) \| base64 -d',line)
assert match and base64.b64decode(match[1])==open(sys.argv[2],'rb').read()
PY
printf 'ok: secret descriptors, trimming, apply results and literal SSH transport\n'
