#!/usr/bin/env bash
# Both chart checks and the deployment skill use this binary resolution policy.
ferry_binary() {
    local mode=${1:-build} root
    root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
    if [ -n "${FERRY_BIN:-}" ]; then
        printf '%s\n' "$FERRY_BIN"
    elif [ "$mode" = build ]; then
        (cd "$root" && cargo build --locked >&2) || return
        printf '%s\n' "$root/target/debug/ferry"
    elif [ "$mode" = existing ]; then
        if [ -x "$root/target/debug/ferry" ]; then printf '%s\n' "$root/target/debug/ferry"
        elif [ -x "$root/target/release/ferry" ]; then printf '%s\n' "$root/target/release/ferry"
        else return 1; fi
    else
        printf 'invalid Ferry binary resolution mode\n' >&2; return 2
    fi
}
