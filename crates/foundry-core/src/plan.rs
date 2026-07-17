use serde::{Deserialize, Serialize};

/// A plan is an executable DAG derived from markdown-plan syntax.
/// This is intentionally minimal; the parser lives separately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub title: String,
    pub tasks: Vec<PlanTask>,
    /// Original lines, preserved so the plan can be written back to disk.
    pub lines: Vec<String>,
}

/// Stable identity of a plan task.
///
/// Graph task keys, job records, and review history all hang off this
/// value, so it must survive plan edits. It comes from an explicit
/// ` - id: <slug>` tag on the task line when present, and is otherwise
/// derived from the task description (never from the line position:
/// positional identity is how displayed task 21 became graph `task-22`
/// and corrupted review targets).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(String);

impl TaskId {
    /// Accepts a lowercase kebab-case slug: `[a-z0-9]` separated by
    /// single dashes, at least two characters.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let valid = raw.len() >= 2
            && raw
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !raw.starts_with('-')
            && !raw.ends_with('-')
            && !raw.contains("--");
        if valid {
            Ok(Self(raw.to_string()))
        } else {
            Err(format!(
                "invalid task id {raw:?}: expected a lowercase kebab-case slug (e.g. `wal-checkpoint`)"
            ))
        }
    }

    /// Derive a stable id from a task description: the first few words,
    /// slugified. Independent of the task's position in the plan.
    pub fn derive(description: &str) -> Self {
        let mut slug = String::new();
        let mut words = 0;
        for word in description.split_whitespace() {
            let cleaned: String = word
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            if cleaned.is_empty() {
                continue;
            }
            if !slug.is_empty() {
                slug.push('-');
            }
            slug.push_str(&cleaned);
            words += 1;
            if words == 5 || slug.len() >= 40 {
                break;
            }
        }
        if slug.len() < 2 {
            slug = format!("task-{:08x}", fnv1a(description.as_bytes()));
        }
        Self(slug)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTask {
    pub id: TaskId,
    /// True when the id came from an explicit ` - id:` tag in the plan
    /// file; false when it was derived from the description and should
    /// be persisted back into the file to become explicit.
    pub id_is_explicit: bool,
    pub description: String,
    pub done: bool,
    pub run: Option<String>,
    pub stop: Option<String>,
    pub files: Vec<String>,
    pub depends_on: Vec<TaskId>,
    pub line_index: usize,
}

/// A task line whose ` - id:` tag is not a valid slug. The task is
/// excluded from the plan rather than silently re-keyed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidTaskId {
    pub line_index: usize,
    pub error: String,
}

const TAG_TERMINATORS: [&str; 4] = [" - run:", " - stop:", " - files:", " - id:"];

impl Plan {
    pub fn parse(title: impl Into<String>, source: &str) -> Self {
        Self::parse_strict(title, source).0
    }

    /// Parse, additionally reporting task lines rejected for invalid
    /// explicit ids. `parse` discards the report for callers that only
    /// render.
    pub fn parse_strict(title: impl Into<String>, source: &str) -> (Self, Vec<InvalidTaskId>) {
        let lines: Vec<String> = source.lines().map(|s| s.to_string()).collect();
        let mut tasks: Vec<PlanTask> = Vec::new();
        let mut invalid = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let Some((rest, done)) = parse_task_marker(trimmed) else {
                continue;
            };
            let run = extract_tag(rest, " - run:", &TAG_TERMINATORS);
            let stop = extract_tag(rest, " - stop:", &TAG_TERMINATORS);
            let files_raw = extract_tag(rest, " - files:", &TAG_TERMINATORS);
            let id_raw = extract_tag(rest, " - id:", &TAG_TERMINATORS);
            let files = files_raw
                .map(|s| s.split(',').map(|f| f.trim().to_string()).collect())
                .unwrap_or_default();

            let first_tag = TAG_TERMINATORS
                .iter()
                .filter_map(|t| rest.find(t))
                .min()
                .unwrap_or(rest.len());
            let description = rest[..first_tag].trim().to_string();

            let (id, id_is_explicit) = match id_raw {
                Some(raw) => match TaskId::parse(&raw) {
                    Ok(id) => (id, true),
                    Err(error) => {
                        invalid.push(InvalidTaskId {
                            line_index: idx,
                            error,
                        });
                        continue;
                    }
                },
                None => (TaskId::derive(&description), false),
            };
            let id = dedupe_id(id, &tasks);

            tasks.push(PlanTask {
                id,
                id_is_explicit,
                description,
                done,
                run,
                stop,
                files,
                depends_on: Vec::new(),
                line_index: idx,
            });
        }
        (
            Self {
                title: title.into(),
                tasks,
                lines,
            },
            invalid,
        )
    }

    pub fn ready_tasks(&self) -> Vec<&PlanTask> {
        self.tasks
            .iter()
            .filter(|t| !t.done && t.depends_on.is_empty())
            .collect()
    }

    pub fn mark_done(&mut self, task_id: &TaskId) -> bool {
        if let Some(task) = self.tasks.iter_mut().find(|t| &t.id == task_id) {
            task.done = true;
            let line = &mut self.lines[task.line_index];
            self.lines[task.line_index] = line.replacen("[ ]", "[x]", 1);
            true
        } else {
            false
        }
    }

    pub fn first_undone(&self) -> Option<&PlanTask> {
        self.tasks.iter().find(|t| !t.done)
    }

    /// Rewrite task lines so every task carries an explicit ` - id:` tag,
    /// returning the ids that were persisted. Idempotent: tasks whose tag
    /// is already explicit are untouched.
    pub fn persist_derived_ids(&mut self) -> Vec<TaskId> {
        let mut persisted = Vec::new();
        for task in &mut self.tasks {
            if task.id_is_explicit {
                continue;
            }
            self.lines[task.line_index].push_str(&format!(" - id: {}", task.id));
            task.id_is_explicit = true;
            persisted.push(task.id.clone());
        }
        persisted
    }

    /// Append a new undone task to the plan, keeping lines and tasks in
    /// sync. The task's stable id is written as an explicit tag.
    pub fn append_task(
        &mut self,
        description: impl Into<String>,
        files: &[String],
        run: Option<&str>,
    ) -> TaskId {
        let next_num = self.tasks.len() + 1;
        let description = description.into();
        let id = dedupe_id(TaskId::derive(&description), &self.tasks);
        let mut line = format!("{}. [ ] {}", next_num, description);
        if !files.is_empty() {
            line.push_str(&format!(" - files: {}", files.join(", ")));
        }
        if let Some(run_cmd) = run {
            line.push_str(&format!(" - run: {}", run_cmd));
        }
        line.push_str(&format!(" - id: {}", id));
        let line_index = self.lines.len();
        self.lines.push(line);
        self.tasks.push(PlanTask {
            id: id.clone(),
            id_is_explicit: true,
            description,
            done: false,
            run: run.map(|s| s.to_string()),
            stop: None,
            files: files.to_vec(),
            depends_on: Vec::new(),
            line_index,
        });
        id
    }
}

/// Two tasks may derive the same slug (identical descriptions). Suffix
/// later occurrences so ids stay unique within a plan.
fn dedupe_id(id: TaskId, existing: &[PlanTask]) -> TaskId {
    if !existing.iter().any(|t| t.id == id) {
        return id;
    }
    for n in 2.. {
        let candidate = TaskId(format!("{}-{}", id.0, n));
        if !existing.iter().any(|t| t.id == candidate) {
            return candidate;
        }
    }
    unreachable!("suffix search always terminates");
}

impl std::fmt::Display for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for line in &self.lines {
            writeln!(f, "{}", line)?;
        }
        Ok(())
    }
}

fn parse_task_marker(line: &str) -> Option<(&str, bool)> {
    // Bullet: "- [ ]" or "- [x]"
    if let Some(rest) = line.strip_prefix("- [ ]") {
        return Some((rest.trim_start(), false));
    }
    if let Some(rest) = line.strip_prefix("- [x]") {
        return Some((rest.trim_start(), true));
    }
    // Numbered: "N. [ ]" or "N. [x]"
    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after_digits = &line[digits.len()..];
    if let Some(rest) = after_digits.strip_prefix(". [ ]") {
        return Some((rest.trim_start(), false));
    }
    if let Some(rest) = after_digits.strip_prefix(". [x]") {
        return Some((rest.trim_start(), true));
    }
    None
}

fn extract_tag(text: &str, prefix: &str, terminators: &[&str]) -> Option<String> {
    let start = text.find(prefix)?;
    let rest = &text[start + prefix.len()..];
    let end = terminators
        .iter()
        .filter_map(|t| rest.find(t))
        .min()
        .unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_id_tag_is_authoritative() {
        let plan = Plan::parse(
            "test",
            "# Test\n1. [ ] Snapshot checkpoints the WAL - run: cargo test - id: wal-checkpoint",
        );
        assert_eq!(plan.tasks[0].id.as_str(), "wal-checkpoint");
        assert!(plan.tasks[0].id_is_explicit);
        assert_eq!(plan.tasks[0].run.as_deref(), Some("cargo test"));
        assert_eq!(plan.tasks[0].description, "Snapshot checkpoints the WAL");
    }

    #[test]
    fn missing_id_is_derived_from_description_not_position() {
        let source_a = "# Test\n1. [ ] Cap the runner output at a named limit";
        let source_b = "# Test\n\n\n7. [ ] Cap the runner output at a named limit";
        let a = Plan::parse("test", source_a);
        let b = Plan::parse("test", source_b);
        assert_eq!(
            a.tasks[0].id, b.tasks[0].id,
            "derived ids must not depend on line position or display number"
        );
        assert_eq!(a.tasks[0].id.as_str(), "cap-the-runner-output-at");
        assert!(!a.tasks[0].id_is_explicit);
    }

    #[test]
    fn duplicate_descriptions_get_distinct_ids() {
        let plan = Plan::parse("test", "# T\n1. [ ] Fix the bug\n2. [ ] Fix the bug");
        assert_eq!(plan.tasks[0].id.as_str(), "fix-the-bug");
        assert_eq!(plan.tasks[1].id.as_str(), "fix-the-bug-2");
    }

    #[test]
    fn invalid_explicit_id_excludes_the_task_and_is_reported() {
        let (plan, invalid) = Plan::parse_strict(
            "test",
            "# T\n1. [ ] Good task - id: ok-task\n2. [ ] Bad - id: Not A Slug",
        );
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].line_index, 2);
        assert!(invalid[0].error.contains("kebab-case"));
    }

    #[test]
    fn task_id_rejects_non_slugs() {
        for bad in [
            "",
            "x",
            "Upper",
            "has space",
            "-lead",
            "trail-",
            "a--b",
            "task_21",
        ] {
            assert!(TaskId::parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(TaskId::parse("wal-checkpoint").is_ok());
        assert!(TaskId::parse("t2").is_ok());
    }

    #[test]
    fn persist_derived_ids_makes_ids_explicit_and_is_idempotent() {
        let mut plan = Plan::parse(
            "test",
            "# T\n1. [ ] Cap the runner output - run: cargo test",
        );
        let persisted = plan.persist_derived_ids();
        assert_eq!(persisted.len(), 1);
        assert!(plan.to_string().contains(
            "1. [ ] Cap the runner output - run: cargo test - id: cap-the-runner-output"
        ));

        // Re-parse: the id must round-trip as explicit and stay stable.
        let reparsed = Plan::parse("test", &plan.to_string());
        assert_eq!(reparsed.tasks[0].id, persisted[0]);
        assert!(reparsed.tasks[0].id_is_explicit);
        assert_eq!(reparsed.tasks[0].run.as_deref(), Some("cargo test"));

        let mut reparsed = reparsed;
        assert!(reparsed.persist_derived_ids().is_empty(), "idempotent");
    }

    #[test]
    fn append_task_adds_numbered_line() {
        let mut plan = Plan::parse("test", "# Test\n1. [ ] First task");
        plan.append_task("Second task", &["a.rs".to_string()], Some("cargo test"));
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[1].description, "Second task");
        assert_eq!(plan.tasks[1].files, vec!["a.rs"]);
        assert_eq!(plan.tasks[1].run, Some("cargo test".to_string()));
        assert!(
            plan.to_string()
                .contains("2. [ ] Second task - files: a.rs - run: cargo test - id: second-task")
        );
    }

    #[test]
    fn append_task_to_empty_plan() {
        let mut plan = Plan::parse("test", "# Test");
        let id = plan.append_task("First task", &[], None);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(id.as_str(), "first-task");
        assert!(plan.tasks[0].id_is_explicit);
        assert!(
            plan.to_string()
                .contains("1. [ ] First task - id: first-task")
        );
    }

    #[test]
    fn mark_done_by_stable_id() {
        let mut plan = Plan::parse(
            "test",
            "# T\n1. [ ] Alpha - id: alpha\n2. [ ] Beta - id: beta",
        );
        assert!(plan.mark_done(&TaskId::parse("alpha").unwrap()));
        assert!(plan.to_string().contains("1. [x] Alpha - id: alpha"));
        assert!(plan.to_string().contains("2. [ ] Beta - id: beta"));
        assert!(!plan.mark_done(&TaskId::parse("missing-task").unwrap()));
    }
}
