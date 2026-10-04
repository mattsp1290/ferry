#!/usr/bin/env bash
set -euo pipefail
command=${1:-}
case "$command" in init|check|publish|preflight|rollout|verify|status|restart|rollback) ;; *) printf 'Usage: deploy <init|check|publish|preflight|rollout|verify|status|restart|rollback> [options]\n' >&2; exit 2 ;; esac
shift
script=$(cd "$(dirname "$0")" && pwd)/cmd/$command.sh
[ -f "$script" ] || { printf 'Command not implemented: %s\n' "$command" >&2; exit 2; }
exec bash "$script" "$@"
