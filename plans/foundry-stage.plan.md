# Software Foundry Vertical Slice

Deliver one trustworthy path from a planned task to an isolated change attempt, captured evidence, and an explicit review decision.

1. [x] Define task and job lifecycle state machines with validated transitions - files: crates/foundry-core/src/job.rs, crates/foundry-core/src/lib.rs - run: cargo test -p foundry-core job::tests
2. [x] Persist jobs and task state transitions in the graph database with idempotency keys - files: crates/foundry-core/src/graph.rs, crates/foundry-core/src/job.rs - run: cargo test -p foundry-core job_persistence
3. [x] Define structured job specifications, results, artifacts, and change sets - files: crates/foundry-core/src/job.rs - run: cargo test -p foundry-core job_contracts
4. [x] Add a non-interactive Podman runner with resource limits, cancellation, and structured output capture - files: crates/foundry-cli/src/main.rs, crates/foundry-core/src/job.rs, justfile - run: cargo test --workspace
5. [x] Capture command logs, changed files, test results, and artifacts as task evidence - files: crates/foundry-core/src/graph.rs, crates/foundry-cli/src/main.rs - run: cargo test --workspace
6. [x] Link jobs, evidence, artifacts, and change sets to their originating tasks - files: crates/foundry-core/src/graph.rs, crates/foundry-cli/src/main.rs - run: cargo test --workspace
7. [x] Add explicit review approve and reject commands with recorded decisions - files: crates/foundry-core/src/graph.rs, crates/foundry-cli/src/main.rs - run: cargo test --workspace
8. [x] Add an end-to-end self-hosting test for plan to evidence to review - files: crates/foundry-cli/tests/vertical_slice.rs - run: cargo test --workspace
