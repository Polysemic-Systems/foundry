//! `foundry evolution` must render the event-timeline projection
//! (`foundry_core::evolution::project`) as plain text: one entry per
//! recorded event, oldest to newest, each entry naming its timestamp,
//! its kind, and the milestones the projection extracts, plus a
//! per-kind count summary over the whole recorded log.
//!
//! The falsifying evidence these tests reject:
//! - the pre-projection implementation dumps raw JSON event payloads
//!   (`{"event": ...}` lines) newest-first, caps the read at 100
//!   events, and prints no summary at all;
//! - a renderer that re-derives chronology and tallies inline from raw
//!   kinds (instead of consuming the projection) produces entry lines
//!   with no milestone content: no durable task key on task lifecycle
//!   lines, no review id on `review_requested`, no job id on
//!   `review_drafted`/`review_resolved`. Raw kinds do not carry that
//!   information, and phase words alone cannot discriminate — every
//!   phase ("planned", "drafted", ...) is a substring of its kind tag —
//!   so only the presence of projection-extracted identities falsifies
//!   an inline re-implementation.

use foundry_core::graph::NodeId;
use foundry_core::job::JobId;
use foundry_core::{Event, Graph};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn temp_root(tag: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("foundry-evolution-{tag}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    root
}

/// Record `events` into a fresh graph in the given order and return every
/// recorded timestamp (RFC 3339) oldest to newest — the chronological
/// contract the CLI output must follow.
fn seed(db: &Path, events: &[Event]) -> Vec<String> {
    let mut graph = Graph::open(db).unwrap();
    for event in events {
        graph.record_event(event).unwrap();
    }
    // `Graph::events` yields the log newest-first; flip it into the
    // oldest-to-newest order the timeline must render.
    let mut stamps: Vec<String> = graph
        .events(10_000)
        .unwrap()
        .into_iter()
        .map(|(at, _)| at.to_rfc3339())
        .collect();
    stamps.reverse();
    assert_eq!(stamps.len(), events.len(), "every seed event was recorded");
    stamps
}

fn run_evolution(db: &Path) -> String {
    let run = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args(["evolution", "--db", db.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "evolution must succeed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    String::from_utf8(run.stdout).unwrap()
}

/// The single output line narrating the event recorded at `stamp` with
/// the given kind.
fn entry_line<'a>(stdout: &'a str, stamp: &str, kind: &str) -> &'a str {
    stdout
        .lines()
        .find(|line| line.contains(stamp) && line.contains(kind))
        .unwrap_or_else(|| panic!("no entry line for {kind} at {stamp}:\n{stdout}"))
}

#[test]
fn entries_are_plain_text_oldest_to_newest_naming_time_and_kind() {
    let root = temp_root("order");
    let db = root.join("foundry.sqlite");
    let events = vec![
        Event::FeatureProposed {
            title: "roadmap".into(),
            plan_path: "plans/roadmap.plan.md".into(),
            task_ids: vec![],
        },
        Event::Deployed {
            target: "staging".into(),
            node_id: NodeId::new(),
        },
        Event::SnapshotCreated {
            name: "pre-iterate".into(),
            path: "snapshots/pre-iterate.sqlite".into(),
        },
    ];
    let stamps = seed(&db, &events);

    let stdout = run_evolution(&db);

    assert!(
        !stdout.contains("{\"event\":"),
        "entries must be rendered as plain text, not JSON payloads:\n{stdout}"
    );
    let kinds = ["feature_proposed", "deployed", "snapshot_created"];
    let mut previous = 0;
    for (stamp, kind) in stamps.iter().zip(kinds) {
        let line = entry_line(&stdout, stamp, kind);
        let position = stdout.find(line).unwrap();
        assert!(
            position >= previous,
            "entries must be ordered oldest to newest: {kind} at {stamp} appears out of order:\n{stdout}"
        );
        previous = position;
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn summary_tallies_every_kind_over_the_recorded_log() {
    let root = temp_root("summary");
    let db = root.join("foundry.sqlite");
    let mut events = vec![
        Event::SnapshotCreated {
            name: "a".into(),
            path: "snapshots/a.sqlite".into(),
        },
        Event::SnapshotCreated {
            name: "b".into(),
            path: "snapshots/b.sqlite".into(),
        },
        Event::Deployed {
            target: "prod".into(),
            node_id: NodeId::new(),
        },
    ];
    for title in ["one", "two", "three"] {
        events.push(Event::FeatureProposed {
            title: title.into(),
            plan_path: format!("plans/{title}.plan.md"),
            task_ids: vec![],
        });
    }
    seed(&db, &events);

    let stdout = run_evolution(&db);

    let summary = stdout
        .split_once("Summary:")
        .unwrap_or_else(|| panic!("a per-kind summary must follow the entries:\n{stdout}"))
        .1;
    for tally in ["deployed: 1", "feature_proposed: 3", "snapshot_created: 2"] {
        assert!(
            summary.contains(tally),
            "summary must tally every recorded kind; missing {tally:?}:\n{stdout}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn timeline_covers_the_whole_log_not_a_tail_of_recent_events() {
    let root = temp_root("whole-log");
    let db = root.join("foundry.sqlite");
    // Well past the historical 100-row display cap: a tail renderer
    // cannot show the oldest entry nor tally the whole log.
    let events: Vec<Event> = (0..150)
        .map(|i| Event::SnapshotCreated {
            name: format!("snap-{i}"),
            path: format!("snapshots/snap-{i}.sqlite"),
        })
        .collect();
    let stamps = seed(&db, &events);

    let stdout = run_evolution(&db);

    assert!(
        stdout.contains(&stamps[0]),
        "the oldest recorded event must appear; a capped tail drops it:\n{stdout}"
    );
    let entries = stdout
        .split_once("Summary:")
        .map(|(entries, _)| entries)
        .unwrap_or(&stdout);
    let entry_count = entries
        .lines()
        .filter(|line| line.contains("snapshot_created"))
        .count();
    assert_eq!(
        entry_count,
        events.len(),
        "every recorded event must render as an entry, not just a recent tail:\n{stdout}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn task_lifecycle_entries_narrate_the_durable_task_key() {
    let root = temp_root("task-milestones");
    let db = root.join("foundry.sqlite");
    let task_id = NodeId::new();
    let events = vec![
        Event::TaskPlanned {
            task_id,
            plan_id: NodeId::new(),
            task_key: "plans/roadmap.plan.md#1".into(),
            plan_path: "plans/roadmap.plan.md".into(),
        },
        Event::TaskStarted {
            task_id,
            task_key: "plans/roadmap.plan.md#1".into(),
            description: "bootstrap the timeline".into(),
        },
        Event::TaskCompleted {
            task_id,
            task_key: "plans/roadmap.plan.md#1".into(),
            description: "bootstrap the timeline".into(),
        },
        Event::TaskFailed {
            task_id: NodeId::new(),
            task_key: "plans/roadmap.plan.md#2".into(),
            description: "harden the renderer".into(),
            reason: "tests red".into(),
        },
    ];
    let stamps = seed(&db, &events);

    let stdout = run_evolution(&db);

    assert!(
        !stdout.contains("{\"event\":"),
        "entries must be rendered as plain text, not JSON payloads:\n{stdout}"
    );
    // The durable task key is the timeline information raw kinds do not
    // carry: only a renderer consuming the projection's milestones can
    // show it. (Phase words alone cannot discriminate — each is a
    // substring of its own kind tag.)
    let expectations = [
        ("task_planned", "plans/roadmap.plan.md#1"),
        ("task_started", "plans/roadmap.plan.md#1"),
        ("task_completed", "plans/roadmap.plan.md#1"),
        ("task_failed", "plans/roadmap.plan.md#2"),
    ];
    for (stamp, (kind, task_key)) in stamps.iter().zip(expectations) {
        let line = entry_line(&stdout, stamp, kind);
        assert!(
            line.contains(task_key),
            "{kind} entry must narrate its durable task key {task_key:?}:\n{line}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn review_entries_narrate_review_and_job_milestones() {
    let root = temp_root("review-milestones");
    let db = root.join("foundry.sqlite");
    let review_id = NodeId::new();
    let job_id = JobId::new();
    let events = vec![
        Event::ReviewRequested {
            review_id,
            task_id: NodeId::new(),
        },
        Event::ReviewDrafted {
            draft_id: uuid::Uuid::new_v4(),
            job_id,
            perspective: "correctness".into(),
        },
        Event::ReviewResolved {
            resolution_id: uuid::Uuid::new_v4(),
            job_id,
            selected_draft_id: None,
        },
    ];
    let stamps = seed(&db, &events);

    let stdout = run_evolution(&db);

    assert!(
        !stdout.contains("{\"event\":"),
        "entries must be rendered as plain text, not JSON payloads:\n{stdout}"
    );
    // A requested review names its graph node; drafting and resolving
    // happen at the job boundary. Both identities come from the
    // projection's milestones, not from the raw kind tags.
    let requested = entry_line(&stdout, &stamps[0], "review_requested");
    assert!(
        requested.contains(&review_id.0.to_string()),
        "review_requested entry must name the review node:\n{requested}"
    );
    for (stamp, kind) in stamps[1..]
        .iter()
        .zip(["review_drafted", "review_resolved"])
    {
        let line = entry_line(&stdout, stamp, kind);
        assert!(
            line.contains(&job_id.0.to_string()),
            "{kind} entry must name the job it belongs to:\n{line}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}
