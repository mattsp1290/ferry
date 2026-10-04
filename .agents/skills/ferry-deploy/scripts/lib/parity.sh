#!/usr/bin/env bash
# Ferry owns git's credential environment, URL derivation, deadlines and redaction.
parity_refs() {
    FERRY_FORGEJO_TOKEN_FILE="$CFG/secrets/forgejo-token" \
    FERRY_GITHUB_TOKEN_FILE="$CFG/secrets/github-token" \
        "$PARITY_FERRY_BIN" refs --config "$PARITY_FERRY_TOML" --side "$1" "$2" 2>/dev/null
}
ref_parity() (
    set -euo pipefail
    umask 077
    local tmp
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-parity.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT
    if ! parity_refs github "$1" > "$tmp/source.raw" ||
       ! parity_refs forgejo "$1" > "$tmp/target.raw"; then
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
repo_has_mirror_topic() {
    with_token_fd "$CFG/secrets/forgejo-token" 3 python3 "$SKILL_DIR/scripts/lib/token-curl.py" \
        "${PARITY_FORGEJO_URL%/}/api/v1/repos/$1/topics" forgejo-get '' | \
        jq -e '.topics | index("ferry-mirror") != null' >/dev/null
}
