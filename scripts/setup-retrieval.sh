#!/usr/bin/env bash
# Optional: build and start the retrieval tools (Loogle and lemma search).
# Needs the image from setup-runtime.sh. Then pass RETRIEVAL=1 to run.sh.
set -euo pipefail
source "$(dirname "$0")/env.sh"
cd "$ROOT"

if docker network inspect "$RETRIEVAL_NETWORK" >/dev/null 2>&1; then
    if [ "$(docker network inspect --format '{{.Internal}}' "$RETRIEVAL_NETWORK")" != true ]; then
        echo "$RETRIEVAL_NETWORK must be an internal Docker network" >&2
        exit 1
    fi
else
    docker network create --internal "$RETRIEVAL_NETWORK" >/dev/null
fi
docker build --build-arg "BEAM_IMAGE=$BEAM_IMAGE" --target client -t "$AGENT_IMAGE" tools/retrieval
docker compose -p "$RETRIEVAL_PROJECT" -f tools/retrieval/compose.yaml up -d --build --wait
