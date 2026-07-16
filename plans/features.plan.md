# Feature Backlog
1. [x] `create_snapshot` creates a new snapshot in the SQLite database and saves it.
2. [x] `snapshot_list` prints all the available snapshots (pathnames) on the filesystem under ~/.foundry/snapshots.
3. [x] `restore_snapshot <snapshot_path>` restores a specific snapshot from the project's SQLite database.
4. [x] Upgrade Graph's existing schema_migrations table in place: preserve applied_at, add checksum storage, backfill every known applied version from the
  canonical migration registry, and test reopening a legacy database - files: crates/foundry-core/src/graph.rs - run: cargo test -p foundry-core
5. [x] Implement a mechanism within `Graph::migrate` to calculate the SHA-256 hash of the canonical SQL string for every executed migration - files: crates/foundry-core/src/graph.rs, crates/foundry-core/src/migration_registry.rs
6. [x] Modify the process that registers applied migrations to persist and associate the calculated checksum with the schema_migrations table entries - files: crates/foundry-core/src/migration_storage.rs, crates/foundry-core/src/db/schema.rs, crates/foundry-core/src/graph.rs
7. [x] Implement a verification function within the doctor logic to retrieve stored checksums for historical migrations and compare them against the current canonical registry - files: crates/foundry-core/src/graph.rs, crates/foundry-core/src/migration_registry.rs, crates/foundry-cli/src/main.rs
8. [x] Add test cases to validate successful checksum storage, tampered data detection, and handling of unknown versions during migration application simulation - run: cargo test
9. [x] Update `crates/foundry-core/src/graph.rs` to define necessary `GraphError` variants and modify the `events()` method to propagate decoding and timestamp errors instead of panicking. - files: crates/foundry-core/src/graph.rs
10. [x] Add a unit test inside `crates/foundry-core/src/graph.rs` under `#[cfg(test)]` that asserts processing an event row with undecodable data or invalid timestamps correctly returns a `GraphError`. This test must verify against the panic behavior. - files: crates/foundry-core/src/graph.rs
11. [x] Modify transition_task to implement a conditional update check based on the current state. - files: crates/foundry-core/src/graph.rs
12. [x] Modify transition_job to implement a conditional update check based on the current state. - files: crates/foundry-core/src/graph.rs
13. [x] Define and incorporate the specific error variant (e.g., StaleTransition or lost-race) into the GraphError enum for use in transitions. - files: crates/foundry-core/src/graph.rs
14. [x] Implement regression tests under `cfg(test)` that simulate concurrent, mismatched state reads to verify the error handling path. - run: cargo test
15. [x] Modify `crates/foundry-cli/src/main.rs` to obtain a mutable graph handle and integrate the logic for emitting a `RuleTriggered` event after each rule execution. - files: crates/foundry-cli/src/main.rs - run: cargo test
16. [x] Implement the code within `crates/foundry-cli/src/main.rs` to call the graph recording mechanism with the rule node ID and result upon completion of checking rules. - files: crates/foundry-cli/src/main.rs - run: cargo test
17. [x] Update the assertion logic within `crates/foundry-core/src/rules.rs` to verify that the newly recorded events are present in the graph history. - files: crates/foundry-core/src/rules.rs - run: cargo test
18. [x] Snapshot create checkpoints the SQLite WAL before copying so a snapshot contains every committed transaction, including ones not yet checkpointed into the main database file - files: crates/foundry-cli/src/main.rs - run: cargo test -p foundry-cli
19. [ ] Cap the runner's captured stdout and stderr at a named byte limit, appending an explicit truncation marker that records how many bytes were dropped, so a runaway job cannot exhaust memory - files: crates/foundry-cli/src/runner.rs - run: cargo test -p foundry-cli
20. [ ] Doctor warns when the evidence blob store contains orphaned objects older than the sweep age guard, reusing the graph's referenced-digest scan - files: crates/foundry-cli/src/main.rs, crates/foundry-core/src/graph.rs - run: cargo test
21. [ ] Record truncation as structured evidence: RunnerOutput and JobResult carry truncated flags and dropped-byte counts per stream instead of splicing the marker into the captured text - files: crates/foundry-cli/src/runner.rs, crates/foundry-core/src/job.rs - run: cargo test
22. [ ] The review TUI edit mode suspends the terminal and opens $EDITOR on the draft, resuming on save, replacing the append-only inline editor - files: crates/foundry-cli/src/main.rs - run: cargo test -p foundry-cli
