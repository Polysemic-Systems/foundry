#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

scripts/gate-fast.sh
cargo test --workspace
