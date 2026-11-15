# Shared settings for the scripts in this directory; source it, don't run it.
# Override any of these by setting them in your environment.
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PACKAGES=${PACKAGES:-$ROOT/lean-packages-dev}
# Keep development builds and services separate from the checked stable checkout.
export BEAM_IMAGE=${BEAM_IMAGE:-eureka-lean:dev}
export AGENT_IMAGE=${AGENT_IMAGE:-eureka-agent:dev}
export RETRIEVAL_IMAGE=${RETRIEVAL_IMAGE:-eureka-retrieval:dev}
export RETRIEVAL_PROJECT=${RETRIEVAL_PROJECT:-eureka-retrieval-dev}
export RETRIEVAL_NETWORK=${RETRIEVAL_NETWORK:-eureka-dev-agents}
HARNESS=$ROOT/harness/target/debug/harness
# User services skip shell startup files. Verification needs elan's Lake and Bubblewrap.
export PATH="${ELAN_HOME:-$HOME/.elan}/bin:$HOME/.local/bin:$PATH"

need_harness() {
    [ -x "$HARNESS" ] || { echo "$HARNESS not found; run scripts/build.sh first" >&2; exit 1; }
    for tool in lake bwrap systemd-run; do
        command -v "$tool" >/dev/null || { echo "$tool not found in PATH" >&2; exit 1; }
    done
}
