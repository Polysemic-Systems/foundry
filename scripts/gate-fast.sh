#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

scripts/sync-readme-status.sh --check
scripts/check-architecture-budgets.sh
scripts/check-source-policies.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --bins -- \
  -D warnings \
  -D clippy::unwrap_used \
  -D clippy::expect_used
cargo clippy -p foundry-core --lib -- \
  -D warnings \
  -D clippy::unwrap_used \
  -D clippy::expect_used
