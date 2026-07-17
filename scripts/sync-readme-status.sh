#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
readme="$root/README.md"
source_file="$root/docs/status.md"
begin='<!-- BEGIN GENERATED STATUS -->'
end='<!-- END GENERATED STATUS -->'
mode="${1:---write}"

if [[ "$mode" != "--write" && "$mode" != "--check" ]]; then
    echo "usage: $0 [--write|--check]" >&2
    exit 2
fi

if [[ "$(grep -Fxc "$begin" "$readme")" -ne 1 || "$(grep -Fxc "$end" "$readme")" -ne 1 ]]; then
    echo "README status markers are missing or duplicated" >&2
    exit 1
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

awk -v begin="$begin" -v end="$end" -v source_file="$source_file" '
    $0 == begin {
        print
        while ((getline line < source_file) > 0) {
            print line
        }
        close(source_file)
        replacing = 1
        next
    }
    $0 == end {
        replacing = 0
        print
        next
    }
    !replacing {
        print
    }
' "$readme" > "$tmp"

if [[ "$mode" == "--check" ]]; then
    if ! diff -u "$readme" "$tmp"; then
        echo "README status is stale; run: scripts/sync-readme-status.sh" >&2
        exit 1
    fi
else
    mv "$tmp" "$readme"
    trap - EXIT
fi
