# Foundry

A local-first, self-building production system.

> Code is the spec. The system builds itself.

## Quick start

```bash
just build      # Build the system
just init       # Create .foundry/ database
just index      # Index this codebase into itself
just plan       # Show the bootstrap plan
just check      # Run all checks
```

## Safe iteration

`foundry iterate` takes the next runnable plan task, executes its `run` command
in a bounded, network-disabled Podman container, and persists the command,
output, changed files, test result, artifacts, and retention metadata. The plan
does not advance until the captured job is explicitly reviewed:

```bash
just iterate
cargo run -p foundry-cli -- review-approve \
  --task 'plans/bootstrap.plan.md#task-1' --job <job-uuid> \
  --reviewer <identity> --reason '<evidence-based decision>'
just iterate  # reflects the approval in the plan and selects the next task
```

Use `review-reject` with the same arguments to return the task to `ready` for a
new attempt. Reusing an idempotency key returns the original immutable result.

## Design

- **One graph**: every artifact is a node (task, code, test, review, deploy, feedback, rule, model, env, plan).
- **Domain languages**: each subsystem owns its vocabulary and types.
- **Hybrid intelligence**: deterministic rules first, local/specialized models second, frontier models last.
- **Internal RAG**: code, plans, failures, and rules are embedded and retrievable locally.
- **Code as spec**: Rust types, schemas, tests, and `just` recipes are the documentation.

## Stack

- Rust 1.92
- SQLite + FTS5 + sqlite-vec (planned)
- Ollama for local models
- Podman/QEMU for runners

## License

GPL-3.0-or-later
