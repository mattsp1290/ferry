#!/usr/bin/env bash
# Shared configuration contract; compatible with bash 3.2.
set -euo pipefail
umask 077
SKILL_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
REPO_ROOT=$(cd "$SKILL_DIR/../../.." && pwd -P)
CFG=${FERRY_DEPLOY_CONFIG:-$HOME/.local/config/ferry}
config_error() { printf '%s\n' "$*" >&2; exit 2; }
if stat -f '%u' "$SKILL_DIR" >/dev/null 2>&1; then
    config_stat() { stat -f "$1" "$2"; }
    STAT_MODE=%Lp
else
    config_stat() { stat -c "$1" "$2"; }
    STAT_MODE=%a
fi
config_pattern() {
    local key=$1 value=$2 pattern
    case "$key" in
        CONTROL_SSH) pattern='^[A-Za-z0-9._][A-Za-z0-9._-]*(@[A-Za-z0-9._][A-Za-z0-9._-]*)?$' ;;
        KUBECONFIG_PATH) pattern='^/[A-Za-z0-9._/-]+$' ;;
        NAMESPACE|RELEASE) pattern='^[a-z0-9]([a-z0-9-]*[a-z0-9])?$'; [ ${#value} -le 63 ] || return 1 ;;
        REGISTRY_PUSH) pattern='^[A-Za-z0-9.-]+(:[0-9]+)?$'; case "$value" in *.*|*:*|localhost) ;; *) return 1 ;; esac ;;
        REGISTRY_INSECURE) pattern='^(true|false)$' ;;
        IMAGE_PULL_REPOSITORY) pattern='^[A-Za-z0-9.-]+(:[0-9]+)?(/[a-z0-9._-]+)+$' ;;
        IMAGE_PUSH_PATH|BASE_PUSH_PATH) pattern='^[a-z0-9._-]+(/[a-z0-9._-]+)*$' ;;
        NODE_SELECTOR) pattern='^[A-Za-z0-9./_-]+=[A-Za-z0-9._-]+$' ;;
        HELM_TIMEOUT) pattern='^[0-9]+[smh]$' ;;
        *) config_error "$CFG/deploy.env: unknown key $key" ;;
    esac
    [[ $value =~ $pattern ]]
}
config_load() {
    local entry relative mode key value line seen=' ' required
    [ -d "$CFG" ] && [ ! -L "$CFG" ] || config_error "$CFG: must be a directory, not a symlink"
    CFG=$(cd "$CFG" && pwd -P)
    while IFS= read -r entry; do
        [ "$(config_stat %u "$entry")" = "$(id -u)" ] || config_error "$entry: must be owned by current user"
        [ ! -L "$entry" ] || config_error "$entry: symlink forbidden"
        relative=${entry#"$CFG"}; relative=${relative#/}
        case "$relative" in
            ''|secrets|state) [ -d "$entry" ] || config_error "$entry: must be a directory" ;;
            deploy.env|values.yaml|secrets/forgejo-token|secrets/github-token|state/last-publish.json|state/preflight.yaml|state/preflight.json|state/deployments.jsonl|state/secret-restart-pending) [ -f "$entry" ] || config_error "$entry: must be a regular file" ;;
            *) config_error "$entry: unexpected entry; remove it" ;;
        esac
        mode=$(config_stat "$STAT_MODE" "$entry")
        if [ -d "$entry" ]; then
            [ "$mode" = 700 ] || config_error "$entry: directory mode must be 0700"
        else
            [ "$mode" = 600 ] || config_error "$entry: file mode must be 0600"
        fi
    done < <(find "$CFG" -print)
    if git -C "$CFG" rev-parse --is-inside-work-tree >/dev/null 2>&1; then config_error "$CFG: must not be inside a git work tree"; fi
    case "$CFG/" in "$REPO_ROOT/"*) config_error "$CFG: must not be under ferry checkout" ;; esac
    [ -f "$CFG/deploy.env" ] || config_error "$CFG/deploy.env: required file missing"
    unset IMAGE_PUSH_PATH BASE_PUSH_PATH NODE_SELECTOR HELM_TIMEOUT
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in ''|'#'*) continue ;; esac
        case "$line" in *=*) key=${line%%=*}; value=${line#*=} ;; *) config_error "$CFG/deploy.env: expected KEY=value" ;; esac
        [[ $key =~ ^[A-Z_]+$ ]] || config_error "$CFG/deploy.env: invalid key"
        case "$seen" in *" $key "*) config_error "$CFG/deploy.env: duplicate key $key" ;; esac
        config_pattern "$key" "$value" || config_error "$CFG/deploy.env: invalid value for $key"
        printf -v "$key" '%s' "$value"
        export "$key"
        seen="$seen$key "
    done < "$CFG/deploy.env"
    for required in CONTROL_SSH KUBECONFIG_PATH NAMESPACE RELEASE REGISTRY_PUSH REGISTRY_INSECURE IMAGE_PULL_REPOSITORY; do
        case "$seen" in *" $required "*) ;; *) config_error "$CFG/deploy.env: missing required key $required" ;; esac
    done
    IMAGE_PUSH_PATH=${IMAGE_PUSH_PATH:-ferry/ferry}; BASE_PUSH_PATH=${BASE_PUSH_PATH:-ferry/base}
    NODE_SELECTOR=${NODE_SELECTOR:-kubernetes.io/arch=amd64}; HELM_TIMEOUT=${HELM_TIMEOUT:-5m}
    [ -s "$CFG/secrets/forgejo-token" ] || config_error "$CFG/secrets/forgejo-token: required non-empty token file"
    [ -f "$CFG/values.yaml" ] || config_error "$CFG/values.yaml: required file missing"
    [ -d "$CFG/state" ] || config_error "$CFG/state: required directory missing"
    export CFG SKILL_DIR REPO_ROOT IMAGE_PUSH_PATH BASE_PUSH_PATH NODE_SELECTOR HELM_TIMEOUT
}
# No token is stored in a variable: only a descriptor is passed to a child.
with_token_fd() {
    local path=$1 fd=$2; shift 2
    case "$fd" in
        3) "$@" 3< "$path" ;; 4) "$@" 4< "$path" ;; 5) "$@" 5< "$path" ;;
        6) "$@" 6< "$path" ;; 7) "$@" 7< "$path" ;; 8) "$@" 8< "$path" ;; 9) "$@" 9< "$path" ;;
        *) config_error 'with_token_fd: descriptor must be 3 through 9' ;;
    esac
}
ferry_binary() {
    if [ -n "${FERRY_BIN:-}" ]; then printf '%s\n' "$FERRY_BIN"; else (cd "$REPO_ROOT" && cargo build --locked >&2); printf '%s\n' "$REPO_ROOT/target/debug/ferry"; fi
}
config_caller=${BASH_SOURCE[1]:-}
if [ -n "$config_caller" ]; then
    config_caller="$(cd "$(dirname "$config_caller")" && pwd -P)/$(basename "$config_caller")"
fi
if [ "$config_caller" != "$SKILL_DIR/scripts/cmd/init.sh" ]; then config_load; fi
