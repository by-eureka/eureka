#!/usr/bin/env bash
# Build the native Beam/Lean image and extract its prepared packages.
# IMAGE defaults to eureka-lean:local; PACKAGES must be a new directory.
set -euo pipefail
source "$(dirname "$0")/env.sh"
cd "$ROOT"
image=${IMAGE:-eureka-lean:local}
if [ -e "$PACKAGES" ]; then
    echo "$PACKAGES already exists; set PACKAGES to a new directory for this runtime" >&2
    exit 1
fi
beam_rev=$(git -C lean-beam rev-parse HEAD)
mapfile -t release < <(python3 - <<'PY'
import json
from pathlib import Path
toolchain = Path('examples/simple/lean-toolchain').read_text().strip()
prefix = 'leanprover/lean4:v'
if not toolchain.startswith(prefix):
    raise SystemExit('the image requires a pinned official Lean release')
version = toolchain[len(prefix):]
checksums = json.loads(Path('harness/deploy/lean-releases.json').read_text())[version]
print(version)
print(checksums['amd64'])
print(checksums['arm64'])
PY
)
[ "${#release[@]}" -eq 3 ] || { echo 'missing pinned Lean release checksums' >&2; exit 1; }
docker build -f harness/deploy/agent.Dockerfile \
    --build-arg "BEAM_REV=$beam_rev" --build-arg "LEAN_VERSION=${release[0]}" \
    --build-arg "LEAN_SHA256_AMD64=${release[1]}" --build-arg "LEAN_SHA256_ARM64=${release[2]}" \
    -t "$image" .
mkdir -p "$(dirname "$PACKAGES")"
container=$(docker create "$image")
staging=$(mktemp -d "${PACKAGES}.staging.XXXXXX")
cleanup() {
    docker rm "$container" >/dev/null
    # Keep failed extraction for inspection; never publish partial packages.
}
trap cleanup EXIT
docker cp "$container:/project/.lake/packages/." "$staging/"
mv "$staging" "$PACKAGES"
echo "Runtime image: $image"
echo "Prepared packages: $PACKAGES"
