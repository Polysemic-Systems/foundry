# Split `main.rs` into focused command modules

The command handlers formerly accumulated in `crates/foundry-cli/src/main.rs` now live in focused modules under `crates/foundry-cli/src/commands/`. The completed extraction reduced `main.rs` from 3,884 lines to 557 lines and left it as a thin argument-parsing and dispatch shell with a 650-line architecture budget.

1. [x] Create `commands/` module skeleton (`commands/mod.rs`) re-exporting command handlers - files: crates/foundry-cli/src/commands/mod.rs - id: create-commands-module-skeleton
2. [x] Extract snapshot create/list/restore helpers into `commands/snapshot.rs` - files: crates/foundry-cli/src/main.rs, crates/foundry-cli/src/commands/snapshot.rs - id: extract-snapshot-commands
3. [x] Extract `ask`, `semsearch`, and Ollama chat/embed helpers into `commands/ask.rs` - files: crates/foundry-cli/src/main.rs, crates/foundry-cli/src/commands/ask.rs - id: extract-ask-commands
4. [x] Extract review approve/reject/TUI, staged promotion, and review-draft generation into `commands/review.rs` - files: crates/foundry-cli/src/main.rs, crates/foundry-cli/src/commands/review.rs - id: extract-review-commands
5. [x] Extract `job-run` and job-result printing into `commands/job.rs` - files: crates/foundry-cli/src/main.rs, crates/foundry-cli/src/commands/job.rs - id: extract-job-commands
6. [x] Extract `iterate`, TDD red/green loop, repair agent, and iteration feedback into `commands/iterate.rs` - files: crates/foundry-cli/src/main.rs, crates/foundry-cli/src/commands/iterate.rs - id: extract-iterate-commands
7. [x] Extract remaining commands (init, plan, list, search, reconcile, reconcile-plan, heal, doctor, check-rules, approve-rule, lease, propose) into `commands/plan.rs` - files: crates/foundry-cli/src/main.rs, crates/foundry-cli/src/commands/plan.rs - id: extract-plan-meta-commands
8. [x] Run the final full workspace tests, clippy, rustfmt, and recursive architecture-budget gate after extraction - run: ./scripts/gate-full.sh - id: run-gates-after-each-slice
