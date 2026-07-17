#!/usr/bin/env bash
set -euo pipefail

if ! command -v rg >/dev/null 2>&1; then
    echo "check-source-policies: rg (ripgrep) is required; refusing to pass without it." >&2
    exit 1
fi

root="$(git rev-parse --show-toplevel)"
cd "$root"
failed=0

# rg exits 0 on match (a policy violation), 1 on no match (compliant), and
# anything else on error. Only exit 1 may pass the gate.
check_absent() {
    local pattern=$1 message=$2
    shift 2
    local status=0
    rg -n "$pattern" "$@" || status=$?
    if ((status == 0)); then
        echo "$message" >&2
        failed=1
    elif ((status != 1)); then
        echo "check-source-policies: rg failed (exit $status) for pattern: $pattern" >&2
        exit 1
    fi
}

check_absent 'Plan::parse(_strict)?\(' \
    "CLI plan paths must use Plan::parse_path or Plan::parse_path_strict." \
    crates/foundry-cli/src --glob '*.rs'

check_absent 'split\([^)]*path[[:space:]]*=' \
    "Structured manifests must be parsed with their format parser, not string splitting." \
    crates --glob '*.rs'

if ((failed)); then
    exit 1
fi
