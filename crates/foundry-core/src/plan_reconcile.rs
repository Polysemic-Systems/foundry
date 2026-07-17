//! Plan/graph reconciliation: a pure diff between what a plan file says
//! and what the graph's durable task state records, in both directions.
//!
//! The graph's task keys are `{plan_path}#{task_id}`. Before task ids
//! were stable, they were derived from markdown line position
//! (`task-{line_index}`), so a header shifted every key off by one and
//! plan edits stranded state under keys no task owns anymore. This
//! module names every divergence class that history produced, and
//! computes the safe migration from legacy positional keys to stable
//! ids. It never touches storage: callers apply what it reports.

use crate::job::TaskState;
use crate::plan::{InvalidTaskId, Plan, TaskId};

/// One legacy positional key (`…#task-{line_index}`) whose line index
/// matches a current task, migratable to that task's stable key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyKeyMigration {
    pub old_key: String,
    pub new_key: String,
    pub task_id: TaskId,
    pub description: String,
    pub state: Option<TaskState>,
}

/// Everything reconciliation can find. `is_clean` is true only when all
/// vectors are empty.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PlanReconcileReport {
    /// Task lines whose explicit ` - id:` tag is not a valid slug; the
    /// parser excluded them, so iteration cannot see them.
    pub invalid_ids: Vec<InvalidTaskId>,
    /// Tasks whose id is derived from the description instead of an
    /// explicit tag. Safe to fix by persisting the tag into the file.
    pub derived_ids: Vec<TaskId>,
    /// Legacy positional keys with an unambiguous stable-id target.
    pub migratable: Vec<LegacyKeyMigration>,
    /// Graph keys under this plan that match no current task and no
    /// legacy mapping (state is `None` when the key exists only in
    /// history tables). Human decision required; never auto-deleted.
    pub orphaned: Vec<(String, Option<TaskState>)>,
    /// Plan says `[ ]` but the graph says Done: the plan file missed an
    /// authoritative completion.
    pub unmarked_done: Vec<TaskId>,
    /// Plan says `[x]` but the graph disagrees (state shown; tasks with
    /// no graph state at all are NOT listed here — a task marked done
    /// before the graph existed is normal history).
    pub marked_done_but_graph_disagrees: Vec<(TaskId, TaskState)>,
}

impl PlanReconcileReport {
    pub fn is_clean(&self) -> bool {
        self.invalid_ids.is_empty()
            && self.derived_ids.is_empty()
            && self.migratable.is_empty()
            && self.orphaned.is_empty()
            && self.unmarked_done.is_empty()
            && self.marked_done_but_graph_disagrees.is_empty()
    }
}

/// Compare a parsed plan against the graph's task state under
/// `plan_relative`. `graph_states` is every `(task_key, state)` pair
/// whose key starts with `{plan_relative}#`.
pub fn reconcile(
    plan_relative: &str,
    plan: &Plan,
    invalid_ids: &[InvalidTaskId],
    graph_states: &[(String, Option<TaskState>)],
) -> PlanReconcileReport {
    let mut report = PlanReconcileReport {
        invalid_ids: invalid_ids.to_vec(),
        ..Default::default()
    };

    let key_of = |id: &TaskId| format!("{plan_relative}#{id}");

    for task in &plan.tasks {
        if !task.id_is_explicit {
            report.derived_ids.push(task.id.clone());
        }
        let state = graph_states
            .iter()
            .find(|(key, _)| key == &key_of(&task.id))
            .and_then(|(_, state)| *state);
        match state {
            Some(TaskState::Done) if !task.done => report.unmarked_done.push(task.id.clone()),
            Some(state) if task.done && state != TaskState::Done => report
                .marked_done_but_graph_disagrees
                .push((task.id.clone(), state)),
            _ => {}
        }
    }

    let current_keys: std::collections::BTreeSet<String> =
        plan.tasks.iter().map(|t| key_of(&t.id)).collect();

    for (key, state) in graph_states {
        if current_keys.contains(key) {
            continue;
        }
        match legacy_target(plan_relative, key, plan) {
            Some(task) => report.migratable.push(LegacyKeyMigration {
                old_key: key.clone(),
                new_key: key_of(&task.id),
                task_id: task.id.clone(),
                description: task.description.clone(),
                state: *state,
            }),
            None => report.orphaned.push((key.clone(), *state)),
        }
    }

    report
}

/// A key is a legacy positional key when its suffix is `task-{n}` and
/// `n` is the markdown line index of exactly one current task (whose own
/// id is not literally `task-{n}` — that would be a current key).
fn legacy_target<'plan>(
    plan_relative: &str,
    key: &str,
    plan: &'plan Plan,
) -> Option<&'plan crate::plan::PlanTask> {
    let suffix = key.strip_prefix(&format!("{plan_relative}#"))?;
    let line_index: usize = suffix.strip_prefix("task-")?.parse().ok()?;
    plan.tasks
        .iter()
        .find(|t| t.line_index == line_index && t.id.as_str() != suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_and_invalid(source: &str) -> (Plan, Vec<InvalidTaskId>) {
        Plan::parse_strict("test", source)
    }

    const PLAN: &str = "\
# Features

1. [x] Checkpoint the WAL - run: cargo test - id: wal-checkpoint
2. [ ] Cap runner output - run: cargo test - id: cap-runner-output
3. [ ] Warn about orphans - run: cargo test
";

    #[test]
    fn clean_when_plan_and_graph_agree_on_stable_keys() {
        let (plan, invalid) =
            plan_and_invalid("# F\n\n1. [x] Alpha - id: alpha\n2. [ ] Beta - id: beta\n");
        let states = vec![
            ("plans/f.plan.md#alpha".to_string(), Some(TaskState::Done)),
            ("plans/f.plan.md#beta".to_string(), Some(TaskState::Ready)),
        ];
        let report = reconcile("plans/f.plan.md", &plan, &invalid, &states);
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn legacy_positional_keys_map_to_stable_ids_by_line_index() {
        let (plan, invalid) = plan_and_invalid(PLAN);
        // Old scheme: header is line 0, blank line 1, tasks at lines 2..4.
        let states = vec![
            ("plans/f.plan.md#task-2".to_string(), Some(TaskState::Done)),
            (
                "plans/f.plan.md#task-3".to_string(),
                Some(TaskState::Review),
            ),
        ];
        let report = reconcile("plans/f.plan.md", &plan, &invalid, &states);
        assert_eq!(report.migratable.len(), 2);
        assert_eq!(report.migratable[0].old_key, "plans/f.plan.md#task-2");
        assert_eq!(
            report.migratable[0].new_key,
            "plans/f.plan.md#wal-checkpoint"
        );
        assert_eq!(report.migratable[1].old_key, "plans/f.plan.md#task-3");
        assert_eq!(
            report.migratable[1].new_key,
            "plans/f.plan.md#cap-runner-output"
        );
        assert!(report.orphaned.is_empty());
    }

    #[test]
    fn positional_key_beyond_any_task_line_is_orphaned() {
        let (plan, invalid) = plan_and_invalid(PLAN);
        let states = vec![(
            "plans/f.plan.md#task-23".to_string(),
            Some(TaskState::Ready),
        )];
        let report = reconcile("plans/f.plan.md", &plan, &invalid, &states);
        assert!(report.migratable.is_empty());
        assert_eq!(report.orphaned, states);
    }

    #[test]
    fn history_only_legacy_key_without_lifecycle_state_is_still_migratable() {
        // Raw repairs have deleted task_states rows while leaving jobs and
        // reviews behind; those keys must still be seen and migrated.
        let (plan, invalid) = plan_and_invalid(PLAN);
        let states = vec![("plans/f.plan.md#task-4".to_string(), None)];
        let report = reconcile("plans/f.plan.md", &plan, &invalid, &states);
        assert_eq!(report.migratable.len(), 1);
        assert_eq!(report.migratable[0].old_key, "plans/f.plan.md#task-4");
        assert_eq!(
            report.migratable[0].new_key,
            "plans/f.plan.md#warn-about-orphans"
        );
        assert_eq!(report.migratable[0].state, None);
    }

    #[test]
    fn unmarked_completion_and_regression_are_both_reported() {
        let (plan, invalid) =
            plan_and_invalid("# F\n\n1. [ ] Alpha - id: alpha\n2. [x] Beta - id: beta\n");
        let states = vec![
            ("plans/f.plan.md#alpha".to_string(), Some(TaskState::Done)),
            ("plans/f.plan.md#beta".to_string(), Some(TaskState::Ready)),
        ];
        let report = reconcile("plans/f.plan.md", &plan, &invalid, &states);
        assert_eq!(report.unmarked_done, vec![TaskId::parse("alpha").unwrap()]);
        assert_eq!(
            report.marked_done_but_graph_disagrees,
            vec![(TaskId::parse("beta").unwrap(), TaskState::Ready)]
        );
    }

    #[test]
    fn task_marked_done_with_no_graph_state_is_normal_history() {
        let (plan, invalid) = plan_and_invalid("# F\n\n1. [x] Alpha - id: alpha\n");
        let report = reconcile("plans/f.plan.md", &plan, &invalid, &[]);
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn derived_and_invalid_ids_are_surfaced() {
        let (plan, invalid) =
            plan_and_invalid("# F\n\n1. [ ] No explicit tag here\n2. [ ] Bad tag - id: NOT-OK\n");
        let report = reconcile("plans/f.plan.md", &plan, &invalid, &[]);
        assert_eq!(report.derived_ids.len(), 1);
        assert_eq!(report.invalid_ids.len(), 1);
        assert!(!report.is_clean());
    }
}
