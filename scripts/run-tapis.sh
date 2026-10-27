#!/usr/bin/env bash
# scripts/run.sh with the Beam pool on Tapis and the model served on Vista.
# Run scripts/connect-tapis.sh first.
#
#   scripts/run-tapis.sh runs/problem runs/mine [extra harness flags...]
#
# Settings (environment), on top of the ones run.sh takes:
#   TAPIS_POOL_TOKEN_FILE  the Tapis pool's token (default: .secrets/tapis-pool.token)
#   IMAGE, PACKAGES        as in connect-tapis.sh (eureka-tapis:local, pool-packages-tapis/)
#   BASE_URL               required: the current Vista endpoint, e.g.
#                          http://vista.tacc.utexas.edu:<port>/v1. The port changes
#                          whenever the deployment restarts, so there is no default.
#   MODEL                  default DeepSeek-V4.1-Flash
#   API_KEY_FILE           file holding that deployment's API key; prompted for if unset
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
: "${PACKAGES:=$ROOT/pool-packages-tapis}"
export PACKAGES
export IMAGE=${IMAGE:-eureka-tapis:local}
export POOL_ENDPOINT=tapis-gateway:9000
export POOL_TOKEN_FILE=${TAPIS_POOL_TOKEN_FILE:-$ROOT/.secrets/tapis-pool.token}
: "${BASE_URL:?set BASE_URL to the current Vista endpoint, e.g. http://vista.tacc.utexas.edu:<port>/v1}"
export BASE_URL
export MODEL=${MODEL:-DeepSeek-V4.1-Flash}
exec "$(dirname "$0")/run.sh" "$@"
