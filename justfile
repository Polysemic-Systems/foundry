set fallback := true
set shell := ["bash", "-cu"]

# Bootstrap the foundry system that builds itself.
# Code is the spec. This file is the executable entry point.

# Show available recipes
help:
    @just --list --unsorted

# Build all crates; extra args forwarded to cargo
build *args:
    cargo build {{ if args == "" { "--workspace" } else { args } }}

# Fast deterministic gate used by pre-commit.
gate-fast:
    ./scripts/gate-fast.sh

# Authoritative gate used by pre-push and CI.
gate-full:
    ./scripts/gate-full.sh

# Backwards-compatible name for the authoritative gate.
check: gate-full

# Enable the versioned local hooks for this checkout.
install-hooks:
    git config core.hooksPath .githooks
    @echo "Installed Foundry hooks from .githooks/"

# Build the mounted workspace in Podman; does not install, publish, or deploy.
sandbox-build:
    just sandbox cargo build --workspace

# Backwards-compatible alias; prefer `just sandbox-build`.
deploy: sandbox-build

# Run the CLI; extra args forwarded to the binary
run *args:
    cargo run -p foundry-cli -- {{args}}

# Run a bounded command in the structured, non-interactive Podman runner.
job-run *args:
    cargo run -p foundry-cli -- job-run -- {{args}}

# Initialize the foundry database for this project
init:
    cargo run -p foundry-cli -- init --root .

# Index the foundry codebase into itself (with embeddings)
index:
    cargo run -p foundry-cli -- index --root . --db ./.foundry/db.sqlite --embed

# Rebuild the graph from source (truncate + re-index, with embeddings)
rebuild:
    cargo run -p foundry-cli -- rebuild --root . --db ./.foundry/db.sqlite --embed

# Semantic code search over embeddings
semsearch query:
    cargo run -p foundry-cli -- semsearch --db ./.foundry/db.sqlite "{{query}}"

# Report drift between graph and filesystem
reconcile:
    cargo run -p foundry-cli -- reconcile --root . --db ./.foundry/db.sqlite

# Detect drift and rebuild if needed
heal:
    cargo run -p foundry-cli -- heal --root . --db ./.foundry/db.sqlite

# Run self-diagnostic checks
doctor:
    cargo run -p foundry-cli -- doctor --root . --db ./.foundry/db.sqlite --plan ./plans/features.plan.md

# Run rule-based diagnostics on the graph
check-rules:
    cargo run -p foundry-cli -- check-rules --db ./.foundry/db.sqlite

# Ask a local model about the codebase
ask query:
    cargo run -p foundry-cli -- ask --db ./.foundry/db.sqlite "{{query}}"

# Execute the next undone task in the active feature plan
iterate:
    @cargo run --quiet -p foundry-cli -- iterate --plan ./plans/features.plan.md --root . --db ./.foundry/db.sqlite

# Execute the next feature task through a test-first editor agent.
# Set FOUNDRY_AGENT_COMMAND, for example: codex exec --full-auto -
iterate-tdd *args:
    @cargo run --quiet -p foundry-cli -- iterate --tdd --plan ./plans/features.plan.md --root . --db ./.foundry/db.sqlite {{args}}

# Answer the two-draft review questionnaire and submit the human resolution.
review-tui task job reviewer:
    @cargo run --quiet -p foundry-cli -- review-tui --root . --db ./.foundry/db.sqlite --task {{quote(task)}} --job {{quote(job)}} --reviewer {{quote(reviewer)}}

# Approve successful job evidence and complete its task.
review-approve task job reviewer reason:
    @cargo run --quiet -p foundry-cli -- review-approve --root . --db ./.foundry/db.sqlite --task {{quote(task)}} --job {{quote(job)}} --reviewer {{quote(reviewer)}} --reason {{quote(reason)}}

# Reject successful job evidence and return its task to ready.
review-reject task job reviewer reason:
    @cargo run --quiet -p foundry-cli -- review-reject --root . --db ./.foundry/db.sqlite --task {{quote(task)}} --job {{quote(job)}} --reviewer {{quote(reviewer)}} --reason {{quote(reason)}}

# Show the bootstrap plan
plan:
    cargo run -p foundry-cli -- plan

# Propose a new feature; foundry discusses it and appends tasks to plans/features.plan.md
propose query *args:
    cargo run -p foundry-cli -- propose {{quote(query)}} {{args}}

# Create a snapshot of the foundry database
snapshot-create *args:
    cargo run -p foundry-cli -- snapshot create {{args}}

# List foundry database snapshots
snapshot-list *args:
    cargo run -p foundry-cli -- snapshot list {{args}}

# Restore a foundry database snapshot
snapshot-restore name *args:
    cargo run -p foundry-cli -- snapshot restore {{quote(name)}} {{args}}

# Start a development loop: watch and test
dev:
    cargo watch -x "test --workspace"

# Container image used by the Podman sandbox
sandbox_image := env("SANDBOX_IMAGE", "docker.io/rust:1.92-bookworm")

# Run a command inside a rootless Podman sandbox (mounts the project at /workspace)
sandbox *args:
    podman run --rm -it \
        --userns=keep-id \
        -v "$(pwd)":/workspace:Z \
        -w /workspace \
        {{sandbox_image}} \
        {{args}}
