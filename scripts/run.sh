#!/usr/bin/env bash
# Start a run in the background.
#
#   scripts/run.sh runs/problem runs/mine [extra harness flags...]
#
# Settings (environment):
#   BASE_URL, MODEL        endpoint and model (default: DeepSeek's API, deepseek-flash)
#   DEEPSEEK_API_KEY       prompted for if neither it nor OPENAI_API_KEY is set
#   API_KEY_FILE           read the API key from this file instead
#   MAX_TURNS              default 200
#   MAX_OUTPUT_TOKENS      default 200000, reasoning included; input plus output
#                          must fit the server's context window
#   IMAGE                  sandbox image (default: beam-pool:local)
#   POOL_ENDPOINT          pool gateway as the sandboxes see it (default: gateway:9000;
#                          tapis-gateway:9000 for Tapis, see run-tapis.sh)
#   ATTACH_TIMEOUT         seconds to attach each sandbox to the pool (default: 300)
#   RETRIEVAL=1            add the tools from setup-retrieval.sh; defaults IMAGE to
#                          eureka-agent:local (a custom IMAGE must include the retrieval client)
#   LOGPROBS=false         explicitly allow missing token probabilities (default: require them)
# Requests, responses, reasoning and stream events are captured for every attempt.
# Run `harness/target/debug/harness --help` for the other flags.
set -euo pipefail
source "$(dirname "$0")/env.sh"
need_harness

if [ $# -lt 2 ]; then
    sed -n '4p' "$0" | sed 's/^#  *//' >&2
    exit 1
fi
problem=$(realpath -- "$1")
run=$(realpath -m -- "$2")
shift 2
if [ -e "$run" ]; then
    echo "$run already exists; use a new run directory" >&2
    exit 1
fi

if [ -n "${API_KEY_FILE:-}" ]; then
    DEEPSEEK_API_KEY=$(cat "$API_KEY_FILE")
    export DEEPSEEK_API_KEY
elif [ -z "${DEEPSEEK_API_KEY:-}" ] && [ -z "${OPENAI_API_KEY:-}" ]; then
    read -rsp 'API key: ' DEEPSEEK_API_KEY
    echo
    export DEEPSEEK_API_KEY
fi

args=(
    --template "$problem" --store "$run/store"
    --packages "$PACKAGES" --pool-token-file "$POOL_TOKEN_FILE"
    --pool-endpoint "${POOL_ENDPOINT:-gateway:9000}"
    --pool-attach-timeout-secs "${ATTACH_TIMEOUT:-300}"
    --base-url "${BASE_URL:-https://api.deepseek.com}" --model "${MODEL:-deepseek-flash}"
    --max-turns "${MAX_TURNS:-200}" --max-output-tokens "${MAX_OUTPUT_TOKENS:-200000}"
    --dont-give-up --trace "$run/trace.jsonl" --capture-requests
    "--logprobs=${LOGPROBS:-true}"
    --beam-skill "$ROOT/lean-beam/skills/lean-beam"
)
if [ "${RETRIEVAL:-0}" = 1 ]; then
    args+=(--image "${IMAGE:-eureka-agent:local}" --mcp "$ROOT/tools/retrieval/mcp.json")
else
    args+=(--image "${IMAGE:-beam-pool:local}")
fi

mkdir -p "$run/store"
nohup "$HARNESS" "${args[@]}" "$@" > "$run/run.log" 2>&1 &
echo $! > "$run/pid"
echo "Started (pid $!). Log: $run/run.log"
echo "Watch it with: scripts/watch.sh $run"
