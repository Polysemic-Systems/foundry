# ADR 0001: Ratcheted module-size budgets

Status: accepted

Foundry's command composition and graph persistence accumulated in
`main.rs` and `graph.rs`, making mutation authority, transaction boundaries,
and test coverage harder to inspect. New Rust modules therefore have an
800-line ceiling. The gate scans Rust modules recursively so extracting code
into a subdirectory cannot escape the policy. Existing oversized modules are
frozen at their current line counts, and any change that grows them fails the
shared gate.

The current exceptions are:

- `crates/foundry-cli/src/main.rs`: 650 lines, preserving it as a thin
  argument-parsing and dispatch shell;
- `crates/foundry-core/src/graph.rs`: 3,500 lines after extracting review and
  job lifecycle persistence into focused submodules; and
- `crates/foundry-cli/src/sweep.rs`: 849 lines, frozen at its current size.

The intended direction is downward: CLI commands move into command modules,
graph capabilities move behind focused persistence interfaces, and tests move
with the behavior they cover. A budget may change only alongside an ADR that
names the new responsibility and explains why a smaller module boundary would
be worse. Refactoring below a ceiling lowers the ceiling in the same change.

The executable policy is `scripts/check-architecture-budgets.sh`. It runs in
the pre-commit hook, pre-push hook, and CI through the same gate scripts.
