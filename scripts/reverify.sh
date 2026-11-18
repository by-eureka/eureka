#!/usr/bin/env bash
# Re-check a saved candidate with Lean’s bundled verifier, without a model. Use the candidate/ directory for multi-file attempts. Publishes it if it passes.
#
#   scripts/reverify.sh runs/problem runs/mine/store/pool-*/attempts/<id>/candidate.lean runs/recheck
#
# To keep working on a candidate instead, start a new run with
# `scripts/run.sh runs/problem runs/next --seed path/to/candidate.lean`.
set -euo pipefail
source "$(dirname "$0")/env.sh"
need_harness

if [ $# -ne 3 ]; then
    sed -n '4p' "$0" | sed 's/^#  *//' >&2
    exit 1
fi
problem=$(realpath -- "$1")
candidate=$(realpath -- "$2")
out=$(realpath -m -- "$3")
if [ -e "$out" ]; then
    echo "$out already exists" >&2
    exit 1
fi

mkdir -p "$out/store"
"$HARNESS" --template "$problem" --store "$out/store" \
    --packages "$PACKAGES" \
    --trace "$out/trace.jsonl" --verify-only "$candidate"
