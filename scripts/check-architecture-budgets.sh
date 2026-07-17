#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
default_max=800
failed=0

budget_for() {
    case "$1" in
        crates/foundry-cli/src/main.rs) echo 650 ;;
        crates/foundry-core/src/graph.rs) echo 3500 ;;
        crates/foundry-cli/src/sweep.rs) echo 849 ;;
        *) echo "$default_max" ;;
    esac
}

while IFS= read -r -d '' path; do
    relative="${path#"$root/"}"
    lines="$(wc -l < "$path")"
    budget="$(budget_for "$relative")"
    if (( lines > budget )); then
        echo "$relative: $lines lines exceeds architecture budget $budget" >&2
        failed=1
    fi
done < <(
    find "$root/crates/foundry-cli/src" "$root/crates/foundry-core/src" \
        -type f -name '*.rs' -print0
)

if (( failed )); then
    echo "Split responsibilities into focused modules; do not raise a ratchet without an ADR." >&2
    exit 1
fi
