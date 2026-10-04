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
NAMESPACE=ferry RELEASE=ferry KUBECONFIG_PATH=/tmp/kube.conf HELM_TIMEOUT=5m IMAGE_PULL_REPOSITORY=localhost:5000/ferry/ferry CONTROL_SSH=admin@control.example.internal
export SKILL_DIR CFG NAMESPACE RELEASE KUBECONFIG_PATH HELM_TIMEOUT IMAGE_PULL_REPOSITORY CONTROL_SSH
config_error() { printf '%s\n' "$*" >&2; exit 2; }
source "$SKILL_DIR/scripts/lib/credentials.sh"
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
shift 6
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
python3 - "$RECORD/argv" "$SKILL_DIR/scripts/remote/apply-secret.sh" "$SKILL_DIR/scripts/remote/lib.sh" <<'PY'
import base64,re,sys
line=open(sys.argv[1]).readline()
match=re.search(r'printf %s ([A-Za-z0-9+/=]+) \| base64 -d',line)
assert match and base64.b64decode(match[1])==open(sys.argv[3],'rb').read()+open(sys.argv[2],'rb').read()
PY
# Invalid input fails before any SSH or kubectl call, including optional tokens.
for scenario in whitespace invalid-utf8 embedded-newline github-invalid; do
  rm -f "$RECORD/applied" "$CFG/state/secret-restart-pending"
  before=$(wc -l < "$RECORD/kubectl")
  printf 'obviously-fake-forgejo\n' > "$CFG/secrets/forgejo-token"
  rm -f "$CFG/secrets/github-token"
  case "$scenario" in
    whitespace) printf ' \t\n' > "$CFG/secrets/forgejo-token" ;;
    invalid-utf8) printf '\377' > "$CFG/secrets/forgejo-token" ;;
    embedded-newline) printf 'fake\nother' > "$CFG/secrets/forgejo-token" ;;
    github-invalid) printf '\377' > "$CFG/secrets/github-token" ;;
  esac
  if apply_secret > "$work/invalid.out" 2>&1; then printf 'invalid token accepted\n' >&2; exit 1; fi
  [ "$(wc -l < "$RECORD/kubectl")" = "$before" ]
  [ ! -f "$RECORD/applied" ]
  [ ! -f "$CFG/state/secret-restart-pending" ]
  if secret_manifest > "$work/invalid.manifest" 2>/dev/null; then exit 1; fi
  [ ! -s "$work/invalid.manifest" ]
done
# API and Secret readers share the same descriptor validation: no malformed
# token may start curl or leave token text in an error message.
cat > "$work/bin/curl" <<'CURL'
#!/usr/bin/env bash
printf 'curl called\n' >> "$RECORD/curl-called"
exit 1
CURL
chmod +x "$work/bin/curl"
for contents in whitespace invalid-utf8 embedded-newline; do
  case "$contents" in
    whitespace) printf ' \t\n' > "$CFG/secrets/forgejo-token" ;;
    invalid-utf8) printf '\377' > "$CFG/secrets/forgejo-token" ;;
    embedded-newline) printf 'obviously-fake\ninvalid' > "$CFG/secrets/forgejo-token" ;;
  esac
  if with_token_fd "$CFG/secrets/forgejo-token" 3 python3 "$SKILL_DIR/scripts/lib/token-curl.py" https://git.example.internal/api/v1/user forgejo ferry > "$work/curl.out" 2>&1; then exit 1; fi
  [ ! -f "$RECORD/curl-called" ]
  grep -q '^Invalid token file contents$' "$work/curl.out"
  if grep -q 'obviously-fake' "$work/curl.out"; then exit 1; fi
done
printf 'ok: secret descriptors, trimming, apply results and literal SSH transport\n'
