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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTask {
    pub id: String,
    pub description: String,
    pub done: bool,
    pub run: Option<String>,
    pub stop: Option<String>,
    pub files: Vec<String>,
    pub depends_on: Vec<String>,
    pub line_index: usize,
}

impl Plan {
    pub fn parse(title: impl Into<String>, source: &str) -> Self {
        let lines: Vec<String> = source.lines().map(|s| s.to_string()).collect();
        let mut tasks = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if let Some((rest, done)) = parse_task_marker(trimmed) {
                let terminators = [" - run:", " - stop:", " - files:"];
                let run = extract_tag(rest, " - run:", &terminators);
                let stop = extract_tag(rest, " - stop:", &terminators);
                let files_raw = extract_tag(rest, " - files:", &terminators);
                let files = files_raw
                    .map(|s| s.split(',').map(|f| f.trim().to_string()).collect())
                    .unwrap_or_default();

                let first_tag = terminators
                    .iter()
                    .filter_map(|t| rest.find(t))
                    .min()
                    .unwrap_or(rest.len());
                let description = rest[..first_tag].trim().to_string();

                tasks.push(PlanTask {
                    id: format!("task-{}", idx),
                    description,
                    done,
                    run,
                    stop,
                    files,
                    depends_on: Vec::new(),
                    line_index: idx,
                });
            }
        }
        Self {
            title: title.into(),
            tasks,
            lines,
        }
    }

    pub fn ready_tasks(&self) -> Vec<&PlanTask> {
        self.tasks
            .iter()
            .filter(|t| !t.done && t.depends_on.is_empty())
            .collect()
    }

    pub fn mark_done(&mut self, task_id: &str) -> bool {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == task_id) {
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

    /// Append a new undone task to the plan, keeping lines and tasks in sync.
    pub fn append_task(
        &mut self,
        description: impl Into<String>,
        files: &[String],
        run: Option<&str>,
    ) {
        let next_num = self.tasks.len() + 1;
        let description = description.into();
        let mut line = format!("{}. [ ] {}", next_num, description);
        if !files.is_empty() {
            line.push_str(&format!(" - files: {}", files.join(", ")));
        }
        if let Some(run_cmd) = run {
            line.push_str(&format!(" - run: {}", run_cmd));
        }
        let line_index = self.lines.len();
        self.lines.push(line);
        self.tasks.push(PlanTask {
            id: format!("task-{}", line_index),
            description,
            done: false,
            run: run.map(|s| s.to_string()),
            stop: None,
            files: files.to_vec(),
            depends_on: Vec::new(),
            line_index,
        });
    }
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
    fn append_task_adds_numbered_line() {
        let mut plan = Plan::parse("test", "# Test\n1. [ ] First task");
        plan.append_task("Second task", &["a.rs".to_string()], Some("cargo test"));
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[1].description, "Second task");
        assert_eq!(plan.tasks[1].files, vec!["a.rs"]);
        assert_eq!(plan.tasks[1].run, Some("cargo test".to_string()));
        assert!(
            plan.to_string()
                .contains("2. [ ] Second task - files: a.rs - run: cargo test")
        );
    }

    #[test]
    fn append_task_to_empty_plan() {
        let mut plan = Plan::parse("test", "# Test");
        plan.append_task("First task", &[], None);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].id, "task-1");
        assert!(plan.to_string().contains("1. [ ] First task"));
    }
}
