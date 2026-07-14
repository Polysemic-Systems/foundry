# Bootstrap Foundry

A local-first, self-building production system.

1. [x] Define the production graph schema - files: crates/foundry-core/src/graph.rs, crates/foundry-core/src/lib.rs
2. [x] Persist the graph in SQLite - files: crates/foundry-core/src/graph.rs
3. [x] Index the foundry codebase into itself - files: crates/foundry-cli/src/main.rs, crates/foundry-core/src/lib.rs
4. [x] Verify the system builds - files: Cargo.toml, justfile, crates/foundry-core/Cargo.toml, crates/foundry-cli/Cargo.toml, rust-toolchain.toml, README.md, Cargo.lock, .gitignore
5. [x] Add deterministic lint rule engine - files: crates/foundry-core/src/rules.rs, crates/foundry-cli/src/main.rs, plans/rule-diagnostics-findings.md
6. [x] Add local model inference via Ollama - files: crates/foundry-cli/src/main.rs - stop: human review
7. [x] Link indexed code to plans/tasks - files: crates/foundry-core/src/plan.rs, crates/foundry-core/src/graph.rs, crates/foundry-cli/src/main.rs - stop: CodeLinkedRule passes without synthetic links
8. [x] Emit RuleTriggered events and represent rules as graph nodes - files: crates/foundry-core/src/event.rs, crates/foundry-core/src/rules.rs, crates/foundry-cli/src/main.rs
9. [x] Add semantic code search - files: crates/foundry-core/src/graph.rs, crates/foundry-core/src/search.rs, crates/foundry-core/src/embed.rs
10. [x] Add a review stop point for rule changes - files: crates/foundry-core/src/plan.rs, crates/foundry-core/src/rules.rs, crates/foundry-core/src/graph.rs, crates/foundry-cli/src/main.rs - stop: reviewer approves new rule
11. [x] Add a runner sandbox with Podman - files: justfile - run: just sandbox cargo --version
12. [x] Deploy foundry to itself - files: justfile - run: just deploy
