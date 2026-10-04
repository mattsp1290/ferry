#!/usr/bin/env bash
set -euo pipefail
[ "$#" -eq 0 ] || { printf 'init takes no arguments\n' >&2; exit 2; }
source "$(dirname "$0")/../lib/config.sh"
[ ! -L "$CFG" ] || config_error "$CFG: symlink forbidden"
case "$CFG/" in "$REPO_ROOT/"*) config_error "$CFG: must not be under ferry checkout" ;; esac
mkdir -p "$CFG"
CFG=$(cd "$CFG" && pwd -P)
case "$CFG/" in "$REPO_ROOT/"*) config_error "$CFG: must not be under ferry checkout" ;; esac
if git -C "$CFG" rev-parse --is-inside-work-tree >/dev/null 2>&1; then config_error "$CFG: must not be inside a git work tree"; fi
for directory in "$CFG" "$CFG/secrets" "$CFG/state"; do
    [ ! -L "$directory" ] || config_error "$directory: symlink forbidden"
    mkdir -p "$directory"
    chmod 700 "$directory"
done
for name in deploy.env values.yaml; do
    if [ ! -e "$CFG/$name" ] && [ ! -L "$CFG/$name" ]; then
        case "$name" in deploy.env) example=deploy.env.example ;; *) example=values.example.yaml ;; esac
        cp "$SKILL_DIR/resources/$example" "$CFG/$name"
        chmod 600 "$CFG/$name"
    fi
done
printf 'Edit %s/deploy.env and %s/values.yaml; create %s/secrets/forgejo-token (0600).\n' "$CFG" "$CFG" "$CFG"
