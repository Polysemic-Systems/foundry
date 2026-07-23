//! The event-timeline projection (`foundry_core::evolution`) turns the
//! append-only event log — which `Graph::events` yields newest-first — into
//! a chronological project history: one entry per recorded event, oldest to
//! newest, carrying the event kind and any task, job, or review milestone
//! the event marks, plus a per-kind count summary over the whole log.

use chrono::{DateTime, Utc};
use foundry_core::evolution::{JobPhase, Milestone, ReviewPhase, TaskPhase, project};
use foundry_core::graph::NodeId;
use foundry_core::{Event, JobId};
use uuid::Uuid;

const T1: &str = "2026-07-01T10:00:00Z";
const T2: &str = "2026-07-01T10:05:00Z";
const T3: &str = "2026-07-01T10:10:00Z";
const T4: &str = "2026-07-01T10:15:00Z";

fn ts(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .expect("test timestamp is valid")
        .with_timezone(&Utc)
}

#[test]
fn empty_log_projects_to_an_empty_timeline() {
    let timeline = project(&[]);
    assert!(timeline.entries.is_empty());
    assert!(timeline.kind_counts.is_empty());
}

#[test]
fn entries_run_oldest_to_newest_even_though_the_log_reads_newest_first() {
    let task_id = NodeId::new();
    // The order `Graph::events` returns: newest first.
    let events = vec![
        (
            ts(T3),
            Event::TaskCompleted {
                task_id,
                task_key: "t-1".into(),
                description: "done".into(),
            },
        ),
        (
            ts(T2),
            Event::TaskStarted {
                task_id,
                task_key: "t-1".into(),
                description: "start".into(),
            },
        ),
        (
            ts(T1),
            Event::NodeCreated {
                node_id: NodeId::new(),
            },
        ),
    ];

    let timeline = project(&events);

    let stamps: Vec<DateTime<Utc>> = timeline.entries.iter().map(|entry| entry.at).collect();
    assert_eq!(stamps, vec![ts(T1), ts(T2), ts(T3)]);
    let kinds: Vec<&str> = timeline.entries.iter().map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        vec!["node_created", "task_started", "task_completed"]
    );
}

#[test]
fn kind_counts_tally_every_recorded_event_not_just_milestones() {
    let task_id = NodeId::new();
    let events = vec![
        (
            ts(T4),
            Event::TaskCompleted {
                task_id,
                task_key: "t-1".into(),
                description: "done".into(),
            },
        ),
        (
            ts(T3),
            Event::SnapshotCreated {
                name: "pre".into(),
                path: "/tmp/pre.db".into(),
            },
        ),
        (
            ts(T2),
            Event::TaskStarted {
                task_id,
                task_key: "t-1".into(),
                description: "start".into(),
            },
        ),
        (
            ts(T1),
            Event::TaskStarted {
                task_id: NodeId::new(),
                task_key: "t-0".into(),
                description: "earlier task".into(),
            },
        ),
    ];

    let timeline = project(&events);

    assert_eq!(timeline.kind_counts.get("task_started"), Some(&2));
    assert_eq!(timeline.kind_counts.get("task_completed"), Some(&1));
    assert_eq!(timeline.kind_counts.get("snapshot_created"), Some(&1));
    assert_eq!(timeline.kind_counts.values().sum::<usize>(), 4);
}

#[test]
fn task_lifecycle_events_carry_task_milestones_with_their_durable_key() {
    let task_id = NodeId::new();
    let plan_id = NodeId::new();
    let events = vec![
        (
            ts(T4),
            Event::TaskFailed {
                task_id,
                task_key: "t-1".into(),
                description: "work".into(),
                reason: "boom".into(),
            },
        ),
        (
            ts(T3),
            Event::TaskCompleted {
                task_id,
                task_key: "t-1".into(),
                description: "work".into(),
            },
        ),
        (
            ts(T2),
            Event::TaskStarted {
                task_id,
                task_key: "t-1".into(),
                description: "work".into(),
            },
        ),
        (
            ts(T1),
            Event::TaskPlanned {
                task_id,
                plan_id,
                task_key: "t-1".into(),
                plan_path: "plans/read-path.plan.md".into(),
            },
        ),
    ];

    let timeline = project(&events);

    assert_eq!(timeline.entries.len(), 4);
    let expected_phases = [
        TaskPhase::Planned,
        TaskPhase::Started,
        TaskPhase::Completed,
        TaskPhase::Failed,
    ];
    for (entry, phase) in timeline.entries.iter().zip(expected_phases) {
        assert_eq!(
            entry.milestones,
            vec![Milestone::Task {
                task_key: Some("t-1".to_string()),
                phase
            }],
        );
    }
}

#[test]
fn legacy_task_events_keep_their_task_milestone_without_a_durable_key() {
    let events = vec![(
        ts(T1),
        Event::TaskStarted {
            task_id: NodeId::new(),
            task_key: String::new(),
            description: "legacy".into(),
        },
    )];

    let timeline = project(&events);

    assert_eq!(
        timeline.entries[0].milestones,
        vec![Milestone::Task {
            task_key: None,
            phase: TaskPhase::Started
        }],
    );
}

#[test]
fn review_events_carry_review_milestones_and_job_progress() {
    let review_id = NodeId::new();
    let task_id = NodeId::new();
    let job_id = JobId(Uuid::new_v4());
    let events = vec![
        (
            ts(T3),
            Event::ReviewResolved {
                resolution_id: Uuid::new_v4(),
                job_id,
                selected_draft_id: None,
            },
        ),
        (
            ts(T2),
            Event::ReviewDrafted {
                draft_id: Uuid::new_v4(),
                job_id,
                perspective: "tests".into(),
            },
        ),
        (ts(T1), Event::ReviewRequested { review_id, task_id }),
    ];

    let timeline = project(&events);

    assert_eq!(
        timeline.entries[0].milestones,
        vec![Milestone::Review {
            review_id: Some(review_id),
            phase: ReviewPhase::Requested
        }],
    );
    // Drafting and resolving are the log's only job-progress signal, and
    // they are review beats at the same time: both milestones are carried.
    assert_eq!(
        timeline.entries[1].milestones,
        vec![
            Milestone::Job {
                job_id,
                phase: JobPhase::Drafted
            },
            Milestone::Review {
                review_id: None,
                phase: ReviewPhase::Drafted
            },
        ],
    );
    assert_eq!(
        timeline.entries[2].milestones,
        vec![
            Milestone::Job {
                job_id,
                phase: JobPhase::Resolved
            },
            Milestone::Review {
                review_id: None,
                phase: ReviewPhase::Resolved
            },
        ],
    );
}

#[test]
fn events_outside_task_job_and_review_lifecycles_carry_no_milestones() {
    let events = vec![
        (
            ts(T2),
            Event::ModelInvoked {
                model: "model-x".into(),
                prompt_tokens: 10,
                cost_usd: 0.01,
            },
        ),
        (
            ts(T1),
            Event::SnapshotCreated {
                name: "pre".into(),
                path: "/tmp/pre.db".into(),
            },
        ),
    ];

    let timeline = project(&events);

    assert_eq!(timeline.entries.len(), 2);
    for entry in &timeline.entries {
        assert!(
            entry.milestones.is_empty(),
            "{} must not be reported as a milestone",
            entry.kind
        );
    }
}
