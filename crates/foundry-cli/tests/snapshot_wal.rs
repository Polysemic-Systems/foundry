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
use std::fs;
use std::process::Command;

fn record_marker(db: &std::path::Path, marker: &str) {
    let mut graph = Graph::open(db).unwrap();
    graph
        .record_event(&Event::ModelInvoked {
            model: marker.to_string(),
            prompt_tokens: 1,
            cost_usd: 0.0,
        })
        .unwrap();
}

fn marker_count(db: &std::path::Path, marker: &str) -> usize {
    Graph::open(db)
        .unwrap()
        .events(1_000)
        .unwrap()
        .into_iter()
        .filter(|(_, event)| matches!(event, Event::ModelInvoked { model, .. } if model == marker))
        .count()
}

fn restore(root: &std::path::Path, db: &std::path::Path, name: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "snapshot",
            "restore",
            "--root",
            root.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--force",
            name,
        ])
        .output()
        .unwrap()
}

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

    // The snapshot is a full copy of the graph and must be private from the
    // moment it exists, not only after the next lease re-hardens the tree.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root.join("snapshots")), 0o700);
        assert_eq!(mode(&root.join("snapshots").join("walsnap.sqlite")), 0o600);
    }

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

#[test]
fn snapshot_restore_rejects_name_traversal() {
    let root = std::env::temp_dir().join(format!(
        "foundry-snapshot-traversal-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(root.join("snapshots")).unwrap();
    let db = root.join("db.sqlite");
    record_marker(&db, "current");
    let escaped = root.join("escape.sqlite");
    record_marker(&escaped, "escaped");

    let output = restore(&root, &db, "../escape");
    assert!(
        !output.status.success(),
        "snapshot names must be one safe name"
    );
    assert_eq!(marker_count(&db, "current"), 1);
    assert_eq!(marker_count(&db, "escaped"), 0);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn snapshot_restore_refuses_while_repository_lease_is_held() {
    let root =
        std::env::temp_dir().join(format!("foundry-snapshot-lease-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("snapshots")).unwrap();
    fs::create_dir_all(root.join(".foundry")).unwrap();
    let db = root.join("db.sqlite");
    record_marker(&db, "current");
    record_marker(&root.join("snapshots/good.sqlite"), "snapshot");

    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".foundry/repository.lease"))
        .unwrap();
    lease.lock().unwrap();
    let output = restore(&root, &db, "good");
    assert!(
        !output.status.success(),
        "restore must refuse while another repository mutation is active"
    );
    assert_eq!(marker_count(&db, "current"), 1);
    assert_eq!(marker_count(&db, "snapshot"), 0);
    std::fs::File::unlock(&lease).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn corrupt_snapshot_is_rejected_without_damaging_current_database() {
    let root =
        std::env::temp_dir().join(format!("foundry-snapshot-corrupt-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("snapshots")).unwrap();
    let db = root.join("db.sqlite");
    record_marker(&db, "current");
    fs::write(
        root.join("snapshots/corrupt.sqlite"),
        b"not a sqlite database",
    )
    .unwrap();

    let output = restore(&root, &db, "corrupt");
    assert!(
        !output.status.success(),
        "corrupt snapshots must be refused"
    );
    assert_eq!(
        marker_count(&db, "current"),
        1,
        "failed validation must leave the destination database usable"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn validated_snapshot_restore_replaces_history_and_records_the_restore() {
    let root =
        std::env::temp_dir().join(format!("foundry-snapshot-restore-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("snapshots")).unwrap();
    let db = root.join("db.sqlite");
    let snapshot = root.join("snapshots/good.sqlite");
    record_marker(&db, "current");
    record_marker(&snapshot, "snapshot");
    let snapshot_before = fs::read(&snapshot).unwrap();

    let output = restore(&root, &db, "good");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(marker_count(&db, "current"), 0);
    assert_eq!(marker_count(&db, "snapshot"), 1);
    assert_eq!(
        fs::read(&snapshot).unwrap(),
        snapshot_before,
        "validation and restore must keep the snapshot immutable"
    );
    let restored_events = Graph::open(&db)
        .unwrap()
        .events(1_000)
        .unwrap()
        .into_iter()
        .filter(
            |(_, event)| matches!(event, Event::SnapshotRestored { name, .. } if name == "good"),
        )
        .count();
    assert_eq!(restored_events, 1);
    fs::remove_dir_all(root).unwrap();
}
