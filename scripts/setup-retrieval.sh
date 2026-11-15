#!/usr/bin/env bash
# Optional: build and start the retrieval tools (Loogle and lemma search).
# Needs the image from setup-runtime.sh. Then pass RETRIEVAL=1 to run.sh.
set -euo pipefail
source "$(dirname "$0")/env.sh"
cd "$ROOT"

if ! docker network inspect harness-agents >/dev/null 2>&1; then
    docker network create --internal harness-agents >/dev/null
fi
docker build --build-arg "BEAM_IMAGE=${BEAM_IMAGE:-eureka-lean:local}" --target client -t eureka-agent:local tools/retrieval
docker compose -f tools/retrieval/compose.yaml up -d --build --wait
