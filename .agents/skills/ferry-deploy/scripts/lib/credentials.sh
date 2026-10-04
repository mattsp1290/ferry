#!/usr/bin/env bash
# No token is stored in a variable: only a descriptor is passed to a child.
with_token_fd() {
    local path=$1 fd=$2; shift 2
    case "$fd" in
        3) "$@" 3< "$path" ;; 4) "$@" 4< "$path" ;;
        *) config_error 'with_token_fd: descriptor must be 3 or 4' ;;
    esac
}
