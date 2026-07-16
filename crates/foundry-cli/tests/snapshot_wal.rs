//! `snapshot create` must capture every committed transaction, including
//! ones still sitting in the SQLite write-ahead log.
//!
//! The graph runs in WAL mode, so a committed transaction lives in the
//! `-wal` file until a checkpoint folds it into the main database file.
//! SQLite only checkpoints on close when the closing connection is the last
//! one anywhere — whenever another foundry process (TUI, watcher, a
//! concurrent command) holds the database open, opening and dropping a
//! connection does not checkpoint. A snapshot taken by copying only the main
//! database file therefore silently drops the most recent committed history.
//! Snapshot create must checkpoint the WAL before copying.

use foundry_core::{Event, Graph};
use std::process::Command;

#[test]
fn snapshot_create_includes_committed_transactions_still_in_the_wal() {
    let root = std::env::temp_dir().join(format!("foundry-snapshot-wal-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("db.sqlite");

    // This connection stands in for another live foundry process. It stays
    // open across the snapshot, so no connection that closes in the meantime
    // is "the last one" and SQLite never checkpoints on close.
    let mut live = Graph::open(&db).unwrap();
    live.record_event(&Event::ModelInvoked {
        model: "wal-marker".to_string(),
        prompt_tokens: 42,
        cost_usd: 0.0,
    })
    .unwrap();

    // Precondition, not the behavior under test: the marker commit is in the
    // WAL, not yet checkpointed into the main database file. If this fails,
    // the scenario no longer exercises the WAL path and the test is vacuous.
    let wal = root.join("db.sqlite-wal");
    let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert!(
        wal_len > 0,
        "expected committed transactions in {wal:?} before snapshotting"
    );

    let create = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "snapshot",
            "create",
            "--root",
            root.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "walsnap",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    drop(live);

    // The snapshot is a bare .sqlite file with no sidecar WAL, so anything
    // left uncheckpointed at copy time is simply gone. Every event committed
    // before the snapshot must be readable from the snapshot alone.
    let snapshot = root.join("snapshots").join("walsnap.sqlite");
    let snap = Graph::open(&snapshot)
        .unwrap_or_else(|e| panic!("snapshot at {snapshot:?} is not a usable database: {e}"));
    let markers = snap
        .events(100)
        .unwrap()
        .into_iter()
        .filter(|(_, event)| {
            matches!(event, Event::ModelInvoked { model, .. } if model == "wal-marker")
        })
        .count();
    assert_eq!(
        markers, 1,
        "snapshot is missing a transaction that was committed (in the WAL) \
         before it was created; snapshot create must checkpoint the WAL \
         before copying the database file"
    );

    std::fs::remove_dir_all(&root).ok();
}
