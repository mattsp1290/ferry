#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../../../.." && pwd)
umask 077
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-parity-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/cfg/secrets"
export CFG="$tmp/cfg" REPO_ROOT="$root" PARITY_FORGEJO_HOST=forge.example PARITY_FORGEJO_USER=ferry PARITY_FORGEJO_URL=https://forge.example PARITY_TEST_ROOT="$tmp"
export PATH="$tmp/bin:$PATH"
cat > "$tmp/bin/git" <<'SH'
#!/usr/bin/env bash
set -eu
[ "$1" = -c ] && [ "$2" = credential.helper= ] && [ "$3" = ls-remote ]
[ "$GIT_ASKPASS" = "$REPO_ROOT/target/debug/ferry" ] && [ "$FERRY_ASKPASS" = 1 ]
[ "$FERRY_ASKPASS_GITHUB_SCHEME" = https ] && [ "$FERRY_ASKPASS_FORGEJO_SCHEME" = https ]
[ "$FERRY_ASKPASS_GITHUB_HOST" = github.com ] && [ "$FERRY_ASKPASS_FORGEJO_HOST" = forge.example ] && [ "$FERRY_ASKPASS_FORGEJO_USER" = ferry ]
[ "$FERRY_FORGEJO_TOKEN_FILE" = "$CFG/secrets/forgejo-token" ] && [ "$FERRY_GITHUB_TOKEN_FILE" = "$CFG/secrets/github-token" ]
[ "$GIT_CONFIG_GLOBAL" = /dev/null ] && [ "$GIT_CONFIG_NOSYSTEM" = 1 ] && [ "$GIT_TERMINAL_PROMPT" = 0 ]
if env | grep -q obviously-fake-token; then exit 1; fi
case "$4" in https://github.com/*) cat "$PARITY_TEST_ROOT/source" ;; *) cat "$PARITY_TEST_ROOT/target" ;; esac
SH
chmod +x "$tmp/bin/git"
source "$root/.agents/skills/ferry-deploy/scripts/lib/parity.sh"
printf 'abcd\trefs/heads/main\n' > "$tmp/source"
cp "$tmp/source" "$tmp/target"
ref_parity example/source example/destination
printf 'efgh\trefs/tags/extra\n' >> "$tmp/source"
if ref_parity example/source example/destination 2> "$tmp/error"; then exit 1; fi
grep -q refs/tags/extra "$tmp/error"
cp "$tmp/target" "$tmp/source"
printf 'efgh\trefs/tags/extra\n' >> "$tmp/target"
if ref_parity example/source example/destination 2> "$tmp/error"; then exit 1; fi
grep -q refs/tags/extra "$tmp/error"

# HTTPS authority normalization agrees with askpass, including default ports.
[ "$(parity_forgejo_host https://FORGE.Example:443/path)" = forge.example ]
[ "$(parity_forgejo_host https://FORGE.Example:8443/path)" = forge.example:8443 ]
[ "$(parity_forgejo_host 'https://[2001:db8::1]:443')" = '[2001:db8::1]' ]
if parity_forgejo_host http://forge.example > "$tmp/out" 2> "$tmp/error"; then exit 1; fi
if parity_forgejo_host https://user:fake@forge.example > "$tmp/out" 2> "$tmp/error"; then exit 1; fi
# The topic API ignores ~/.curlrc, refuses plain HTTP, and bounds its request.
cat > "$tmp/bin/curl" <<'SH'
#!/usr/bin/env bash
set -eu
[ "$1" = --disable ]
case " $* " in *" --proto =https "*) ;; *) exit 1 ;; esac
case " $* " in *" --max-time 30 "*) ;; *) exit 1 ;; esac
cat > /dev/null
printf '%s\n' '{"topics":["ferry-mirror"]}'
SH
chmod +x "$tmp/bin/curl"
printf 'obviously-fake-token\n' > "$CFG/secrets/forgejo-token"
with_token_fd() { local path=$1 fd=$2; shift 2; [ "$fd" = 9 ]; "$@" 9< "$path"; }
repo_has_mirror_topic example/destination
