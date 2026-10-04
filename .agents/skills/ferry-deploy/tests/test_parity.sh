#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../../../.." && pwd)
umask 077
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ferry-parity-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/cfg/secrets"
export SKILL_DIR="$root/.agents/skills/ferry-deploy" CFG="$tmp/cfg" REPO_ROOT="$root" PARITY_FORGEJO_URL=https://forge.example PARITY_TEST_ROOT="$tmp"
export PATH="$tmp/bin:$PATH"
cat > "$tmp/bin/ferry" <<'SH'
#!/usr/bin/env bash
set -eu
[ "$1" = refs ] && [ "$2" = --config ] && [ "$3" = "$PARITY_FERRY_TOML" ] && [ "$4" = --side ]
[ "$FERRY_FORGEJO_TOKEN_FILE" = "$CFG/secrets/forgejo-token" ] && [ "$FERRY_GITHUB_TOKEN_FILE" = "$CFG/secrets/github-token" ]
if env | grep -q obviously-fake-token; then exit 1; fi
case "$5" in github) cat "$PARITY_TEST_ROOT/source" ;; forgejo) cat "$PARITY_TEST_ROOT/target" ;; *) exit 1 ;; esac
SH
chmod +x "$tmp/bin/ferry"
export PARITY_FERRY_BIN="$tmp/bin/ferry" PARITY_FERRY_TOML="$tmp/live.toml"
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

# The topic API ignores ~/.curlrc, refuses plain HTTP, and bounds its request.
cat > "$tmp/bin/curl" <<'SH'
#!/usr/bin/env bash
set -eu
[ "$1" = --disable ]
case " $* " in *" --proto =https "*) ;; *) exit 1 ;; esac
case " $* " in *" --max-time 30 "*) ;; *) exit 1 ;; esac
case " $* " in *" --connect-timeout 15 "*) ;; *) exit 1 ;; esac
while [ "$#" -gt 0 ]; do
  if [ "$1" = --config ]; then shift; cat "$1" >/dev/null; fi
  shift
done
printf '%s\n200' '{"topics":["ferry-mirror"]}'
SH
chmod +x "$tmp/bin/curl"
printf 'obviously-fake-token\n' > "$CFG/secrets/forgejo-token"
source "$root/.agents/skills/ferry-deploy/scripts/lib/credentials.sh"
repo_has_mirror_topic example/destination
