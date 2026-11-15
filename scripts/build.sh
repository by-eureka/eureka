#!/usr/bin/env bash
# Build the harness. Verification uses the template's Lean 4.35+ toolchain.
set -euo pipefail
source "$(dirname "$0")/env.sh"
cd "$ROOT"
cargo build --locked --manifest-path harness/Cargo.toml
