# Feature Backlog
1. [x] `create_snapshot` creates a new snapshot in the SQLite database and saves it.
2. [x] `snapshot_list` prints all the available snapshots (pathnames) on the filesystem under ~/.foundry/snapshots.
3. [x] `restore_snapshot <snapshot_path>` restores a specific snapshot from the project's SQLite database.
4. [ ] Upgrade Graph's existing schema_migrations table in place: preserve applied_at, add checksum storage, backfill every known applied version from the
  canonical migration registry, and test reopening a legacy database - files: crates/foundry-core/src/graph.rs - run: cargo test -p foundry-core
5. [ ] Implement a mechanism within `Graph::migrate` to calculate the SHA-256 hash of the canonical SQL string for every executed migration - files: crates/foundry-core/src/graph.rs, crates/foundry-core/src/migration_registry.rs
6. [ ] Modify the process that registers applied migrations to persist and associate the calculated checksum with the schema_migrations table entries - files: crates/foundry-core/src/migration_storage.rs, crates/foundry-core/src/db/schema.rs, crates/foundry-core/src/graph.rs
7. [ ] Implement a verification function within the doctor logic to retrieve stored checksums for historical migrations and compare them against the current canonical registry - files: crates/foundry-core/src/graph.rs, crates/foundry-core/src/migration_registry.rs, crates/foundry-cli/src/main.rs
8. [ ] Add test cases to validate successful checksum storage, tampered data detection, and handling of unknown versions during migration application simulation - run: cargo test
