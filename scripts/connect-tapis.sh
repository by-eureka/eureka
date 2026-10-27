#!/usr/bin/env bash
# Connect the sandboxes to the Beam pool on Tapis and check that it answers.
#
#   scripts/connect-tapis.sh
#
# Starts the bridge, prints the pool's status, gets a sandbox image that matches
# the Tapis snapshot if there isn't one yet, and extracts that image's packages.
# On amd64 the image is the one the Tapis workers run, pulled and tagged as is.
# On other platforms it is built by build-tapis-image.sh. Safe to rerun.
#
# Settings (environment):
#   TAPIS_GATEWAY          gateway host (default: infinileanpool.pods.tacc.tapis.io).
#                          `lean-beam/deploy/beam-pool/tapis.py status` prints it.
#   TAPIS_POOL_TOKEN_FILE  the pool's BEAM_POOL_TOKEN (default: .secrets/tapis-pool.token)
#   TAPIS_SOURCE_IMAGE     the image the Tapis workers run (default: debganguly/infinilean:0.0.1)
#   IMAGE                  sandbox image (default: eureka-tapis:local)
#   PACKAGES               where its packages go (default: pool-packages-tapis/)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
: "${PACKAGES:=$ROOT/pool-packages-tapis}"
source "$ROOT/scripts/env.sh"
cd "$ROOT"
export TAPIS_GATEWAY=${TAPIS_GATEWAY:-infinileanpool.pods.tacc.tapis.io}
export TAPIS_POOL_TOKEN_FILE=${TAPIS_POOL_TOKEN_FILE:-$ROOT/.secrets/tapis-pool.token}
IMAGE=${IMAGE:-eureka-tapis:local}
TAPIS_SOURCE_IMAGE=${TAPIS_SOURCE_IMAGE:-debganguly/infinilean:0.0.1}

# A fresh checkout may never have started a local pool. The bridge and snapshot
# checks still need its base image, but no local workers need to be started.
# On amd64 the Tapis image already has the same layout, so pulling it replaces
# the much slower local build.
if ! docker image inspect beam-pool:local >/dev/null 2>&1; then
    if [ "$(uname -m)" = x86_64 ]; then
        docker pull "$TAPIS_SOURCE_IMAGE"
        docker tag "$TAPIS_SOURCE_IMAGE" beam-pool:local
    else
        docker build -f lean-beam/deploy/beam-pool/Dockerfile -t beam-pool:local lean-beam
    fi
fi

# The local pool creates this network too; create it here if that pool isn't used.
if ! docker network inspect harness-agents >/dev/null 2>&1; then
    docker network create --internal harness-agents >/dev/null
fi
docker compose -f harness/deploy/tapis-bridge.yaml up -d

# Passed by name so the token doesn't appear in the process list.
BEAM_POOL_TOKEN=$(cat "$TAPIS_POOL_TOKEN_FILE")
export BEAM_POOL_TOKEN
docker run --rm --network harness-agents -w /opt/beam -e PYTHONPATH=/opt/beam -e BEAM_POOL_TOKEN \
    --entrypoint python3 beam-pool:local -m scripts.beam_pool status --endpoint tapis-gateway:9000
echo

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    if [ "$(uname -m)" = x86_64 ]; then
        docker pull "$TAPIS_SOURCE_IMAGE"
        docker tag "$TAPIS_SOURCE_IMAGE" "$IMAGE"
    else
        OUTPUT_IMAGE=$IMAGE "$ROOT/scripts/build-tapis-image.sh"
    fi
fi
if [ ! -d "$PACKAGES" ]; then
    docker create --name pool-extract "$IMAGE" >/dev/null
    trap 'docker rm pool-extract >/dev/null' EXIT
    docker cp pool-extract:/project/.lake/packages "$PACKAGES"
fi
echo "Connected. Start a run with: scripts/run-tapis.sh runs/problem runs/mine"
