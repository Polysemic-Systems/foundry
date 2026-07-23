//! The event-timeline projection: turns the append-only event log — which
//! [`Graph::events`](crate::graph::Graph::events) yields newest-first — into
//! a chronological project history.
//!
//! One entry per recorded event, oldest to newest, carrying the event kind
//! and any task, job, or review milestone the event marks, plus a per-kind
//! count summary over the whole log. This is a pure read projection over
//! data the system already writes; nothing here mutates the log.

use crate::event::Event;
use crate::graph::NodeId;
use crate::job::JobId;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// A chronological project history projected from the event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    /// One entry per recorded event, oldest to newest.
    pub entries: Vec<TimelineEntry>,
    /// How many events of each kind the log holds, milestones or not.
    pub kind_counts: BTreeMap<&'static str, usize>,
}

/// One recorded event placed in chronological order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEntry {
    /// When the event was recorded.
    pub at: DateTime<Utc>,
    /// The event's stable kind tag (see [`Event::kind`]).
    pub kind: &'static str,
    /// The task, job, and review milestones this event marks. Empty for
    /// events outside those lifecycles.
    pub milestones: Vec<Milestone>,
}

/// A lifecycle beat an event marks in project history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Milestone {
    /// A task lifecycle beat, keyed by the durable plan-relative task key
    /// when the recording Foundry version wrote one (legacy rows: `None`).
    Task {
        task_key: Option<String>,
        phase: TaskPhase,
    },
    /// A job progress beat. Drafting and resolving reviews are the log's
    /// only job-progress signal.
    Job { job_id: JobId, phase: JobPhase },
    /// A review lifecycle beat. Only a requested review names its graph
    /// node; drafting and resolving happen at the job boundary instead.
    Review {
        review_id: Option<NodeId>,
        phase: ReviewPhase,
    },
}

/// Where a task stands in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPhase {
    Planned,
    Started,
    Completed,
    Failed,
}

/// Where a job stands, as far as the event log can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    Drafted,
    Resolved,
}

/// Where a review stands in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewPhase {
    Requested,
    Drafted,
    Resolved,
}

/// Project the recorded event log into a chronological timeline.
///
/// Accepts the log in any order — `Graph::events` yields it newest-first —
/// and returns entries oldest to newest. `kind_counts` tallies every
/// recorded event, not just the ones that mark milestones.
pub fn project(events: &[(DateTime<Utc>, Event)]) -> Timeline {
    let mut chronological: Vec<&(DateTime<Utc>, Event)> = events.iter().collect();
    chronological.sort_by_key(|(at, _)| *at);

    let mut kind_counts = BTreeMap::new();
    let entries = chronological
        .into_iter()
        .map(|(at, event)| {
            *kind_counts.entry(event.kind()).or_insert(0) += 1;
            TimelineEntry {
                at: *at,
                kind: event.kind(),
                milestones: milestones_of(event),
            }
        })
        .collect();

    Timeline {
        entries,
        kind_counts,
    }
}

/// The milestones one event marks, in the order history should narrate
/// them: job progress before the review beat it rides on.
fn milestones_of(event: &Event) -> Vec<Milestone> {
    let task = |phase: TaskPhase, event: &Event| {
        vec![Milestone::Task {
            task_key: event.durable_task_key().map(str::to_string),
            phase,
        }]
    };
    match event {
        Event::TaskPlanned { .. } => task(TaskPhase::Planned, event),
        Event::TaskStarted { .. } => task(TaskPhase::Started, event),
        Event::TaskCompleted { .. } => task(TaskPhase::Completed, event),
        Event::TaskFailed { .. } => task(TaskPhase::Failed, event),
        Event::ReviewRequested { review_id, .. } => vec![Milestone::Review {
            review_id: Some(*review_id),
            phase: ReviewPhase::Requested,
        }],
        Event::ReviewDrafted { job_id, .. } => vec![
            Milestone::Job {
                job_id: *job_id,
                phase: JobPhase::Drafted,
            },
            Milestone::Review {
                review_id: None,
                phase: ReviewPhase::Drafted,
            },
        ],
        Event::ReviewResolved { job_id, .. } => vec![
            Milestone::Job {
                job_id: *job_id,
                phase: JobPhase::Resolved,
            },
            Milestone::Review {
                review_id: None,
                phase: ReviewPhase::Resolved,
            },
        ],
        _ => Vec::new(),
    }
}
