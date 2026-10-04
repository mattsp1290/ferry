#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../../.."
python3 .agents/skills/ferry-deploy/tests/lib/environment_scan.py
