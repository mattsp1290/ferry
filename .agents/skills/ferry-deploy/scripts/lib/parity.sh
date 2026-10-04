#!/usr/bin/env bash
# Credentials remain in files; ferry's askpass mode is the only reader for git.
# Match URL normalization used by ferry's askpass::host_port.
parity_forgejo_host() {
    python3 -c 'import sys, urllib.parse
try:
    url = urllib.parse.urlsplit(sys.argv[1])
    if url.scheme != "https" or not url.hostname or url.username is not None or url.password is not None:
        raise ValueError()
    host = url.hostname.encode("idna").decode("ascii").lower()
    if ":" in host:
        host = "[" + host + "]"
    port = url.port
    print(host + (":" + str(port) if port is not None and port != 443 else ""))
except ValueError:
    sys.exit("Invalid Forgejo HTTPS URL")' "$1"
}
parity_git() {
    GIT_ASKPASS="${PARITY_FERRY_BIN:-${FERRY_BIN:-$REPO_ROOT/target/debug/ferry}}" \
    FERRY_ASKPASS=1 FERRY_ASKPASS_GITHUB_HOST=github.com FERRY_ASKPASS_GITHUB_SCHEME=https \
    FERRY_ASKPASS_FORGEJO_HOST="$PARITY_FORGEJO_HOST" FERRY_ASKPASS_FORGEJO_SCHEME=https \
    FERRY_ASKPASS_FORGEJO_USER="$PARITY_FORGEJO_USER" \
    FERRY_FORGEJO_TOKEN_FILE="$CFG/secrets/forgejo-token" \
    FERRY_GITHUB_TOKEN_FILE="$CFG/secrets/github-token" \
    GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 \
    git -c credential.helper= ls-remote "$1" 'refs/heads/*' 'refs/tags/*' 2>/dev/null
}
ref_parity() (
    set -euo pipefail
    umask 077
    local tmp
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-parity.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT
    if ! parity_git "https://github.com/$1.git" > "$tmp/source.raw" ||
       ! parity_git "${PARITY_FORGEJO_URL%/}/$2.git" > "$tmp/target.raw"; then
        printf 'FAIL: mirror refs unavailable\n' >&2
        return 1
    fi
    LC_ALL=C sort "$tmp/source.raw" > "$tmp/source"
    LC_ALL=C sort "$tmp/target.raw" > "$tmp/target"
    if ! cmp -s "$tmp/source" "$tmp/target"; then
        # Emit ref names only, never URLs, credentials, or git stderr.
        diff "$tmp/source" "$tmp/target" | awk '/^[<>] / {print "FAIL: differing ref " $3}' >&2
        return 1
    fi
)
# curl reads the Authorization header on a descriptor, never argv/environment.
parity_topic_request() {
    python3 -c 'import os,sys
with os.fdopen(9, "rb", closefd=False) as stream:
    token = stream.read().decode().rstrip()
if not token or any(c in token for c in "\r\n\x00"):
    sys.exit("Invalid token file contents")
sys.stdout.write("header = \"Authorization: token " + token.replace("\\", "\\\\").replace("\"", "\\\"") + "\"\n")' |
        curl --disable --silent --show-error --fail --proto '=https' --max-time 30 --config /dev/stdin "$1"
}
repo_has_mirror_topic() {
    with_token_fd "$CFG/secrets/forgejo-token" 9 parity_topic_request \
        "${PARITY_FORGEJO_URL%/}/api/v1/repos/$1/topics" | jq -e '.topics | index("ferry-mirror") != null' >/dev/null
}
