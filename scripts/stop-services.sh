#!/usr/bin/env bash
# Stop optional retrieval. Attempt containers are owned and stopped by each run.
set -euo pipefail
source "$(dirname "$0")/env.sh"
cd "$ROOT"
docker compose -f tools/retrieval/compose.yaml down
