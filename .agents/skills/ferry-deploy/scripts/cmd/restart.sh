#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")/../lib" && pwd)/config.sh"
source "$SKILL_DIR/scripts/lib/remote.sh"
[ "$#" = 0 ] || exit 2
remote_run restart
rm -f "$CFG/state/secret-restart-pending"
