use anyhow::{Context, Result};
use foundry_core::Graph;
use foundry_core::evolution::{
    JobPhase, Milestone, ReviewPhase, TaskPhase, TimelineEntry, project,
};
use std::path::Path;

/// The timeline is a narration of the whole recorded log, not a tail of
/// recent rows: history is the point of the command. `Graph::events` is
/// limit-bounded, so ask for every row SQLite can return.
const WHOLE_LOG: usize = i64::MAX as usize;

pub fn cmd_evolution(db: &Path) -> Result<()> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    let events = graph
        .events(WHOLE_LOG)
        .with_context(|| format!("reading event log from {:?}", db))?;
    if events.is_empty() {
        println!("No events recorded.");
        return Ok(());
    }
    // The projection owns chronology, milestone extraction, and the
    // per-kind tally; the CLI only renders it as plain text.
    let timeline = project(&events);
    for entry in &timeline.entries {
        println!("{} {}", entry.at.to_rfc3339(), narrate(entry));
    }
    println!("Summary:");
    for (kind, count) in &timeline.kind_counts {
        println!("{kind}: {count}");
    }
    Ok(())
}

/// One entry as plain text: the kind tag followed by the task, job, and
/// review milestones the projection extracted from the event.
fn narrate(entry: &TimelineEntry) -> String {
    let mut line = entry.kind.to_string();
    for milestone in &entry.milestones {
        line.push(' ');
        line.push_str(&narrate_milestone(milestone));
    }
    line
}

fn narrate_milestone(milestone: &Milestone) -> String {
    match milestone {
        Milestone::Task { task_key, phase } => {
            let phase = match phase {
                TaskPhase::Planned => "planned",
                TaskPhase::Started => "started",
                TaskPhase::Completed => "completed",
                TaskPhase::Failed => "failed",
            };
            match task_key {
                Some(task_key) => format!("task {phase} ({task_key})"),
                None => format!("task {phase}"),
            }
        }
        Milestone::Job { job_id, phase } => {
            let phase = match phase {
                JobPhase::Drafted => "drafted",
                JobPhase::Resolved => "resolved",
            };
            format!("job {phase} ({})", job_id.0)
        }
        Milestone::Review { review_id, phase } => {
            let phase = match phase {
                ReviewPhase::Requested => "requested",
                ReviewPhase::Drafted => "drafted",
                ReviewPhase::Resolved => "resolved",
            };
            match review_id {
                Some(review_id) => format!("review {phase} ({})", review_id.0),
                None => format!("review {phase}"),
            }
        }
    }
}
