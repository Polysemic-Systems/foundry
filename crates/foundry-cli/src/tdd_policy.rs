use crate::runner;
use anyhow::{Context, Result, bail};
use std::path::Path;

pub fn validate_red_phase_changes(change_set: &foundry_core::ChangeSet) -> Result<()> {
    if change_set.files.is_empty() {
        bail!("TDD red phase did not add or modify a test");
    }
    let invalid = change_set
        .files
        .iter()
        .filter(|change| !red_change_is_test_only(change))
        .map(|change| change.path.as_str())
        .collect::<Vec<_>>();
    if !invalid.is_empty() {
        bail!(
            "TDD red phase changed production files before proving the missing behavior: {}",
            invalid.join(", ")
        );
    }
    Ok(())
}

/// The red phase may add its test to an existing `#[cfg(test)]` section of an
/// implementation file, and the green phase may then need to edit that same
/// file — demanding byte-identity would forbid legal implementations. The
/// enforced invariant is line survival: every line the red phase added must
/// still be present after green. Dedicated test files must stay untouched.
pub fn validate_green_preserves_red(root: &Path, red: &foundry_core::ChangeSet) -> Result<()> {
    let current = runner::snapshot_workspace(root)
        .context("capturing workspace after the TDD green phase")?;
    let mut violations = Vec::new();
    for change in &red.files {
        let Some(after) = change.after.as_ref() else {
            continue;
        };
        let current_evidence = current.get(&change.path);
        if current_evidence == Some(after) {
            continue;
        }
        let dedicated_test_file = Path::new(&change.path)
            .components()
            .any(|component| component.as_os_str() == "tests");
        if dedicated_test_file {
            violations.push(change.path.as_str());
            continue;
        }
        let Some(current_evidence) = current_evidence else {
            violations.push(change.path.as_str());
            continue;
        };
        let before_text = change
            .before
            .as_ref()
            .map(|evidence| String::from_utf8_lossy(&evidence.bytes).into_owned())
            .unwrap_or_default();
        let after_text = String::from_utf8_lossy(&after.bytes).into_owned();
        let current_text = String::from_utf8_lossy(&current_evidence.bytes).into_owned();
        let before_lines: std::collections::BTreeSet<&str> = before_text.lines().collect();
        let current_lines: std::collections::BTreeSet<&str> = current_text.lines().collect();
        let red_added_missing = after_text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter(|line| !before_lines.contains(line))
            .any(|line| !current_lines.contains(line));
        if red_added_missing {
            violations.push(change.path.as_str());
        }
    }
    if !violations.is_empty() {
        bail!(
            "TDD green phase changed or removed the test that established the red failure: {}",
            violations.join(", ")
        );
    }
    Ok(())
}

fn red_change_is_test_only(change: &foundry_core::ChangedFile) -> bool {
    let path = Path::new(&change.path);
    let path_text = change.path.to_ascii_lowercase();
    if path
        .components()
        .any(|component| component.as_os_str() == "tests")
        || path_text.ends_with("_test.rs")
        || path_text.ends_with(".test.rs")
        || path_text.ends_with("_spec.rs")
        || path_text.ends_with(".spec.rs")
    {
        return change.status != foundry_core::ChangeStatus::Deleted;
    }

    if path.extension().and_then(|extension| extension.to_str()) != Some("rs")
        || change.status != foundry_core::ChangeStatus::Modified
    {
        return false;
    }
    let Some(before) = change.before.as_ref() else {
        return false;
    };
    let Some(after) = change.after.as_ref() else {
        return false;
    };
    let Ok(before) = std::str::from_utf8(&before.bytes) else {
        return false;
    };
    let Ok(after) = std::str::from_utf8(&after.bytes) else {
        return false;
    };
    let marker = "#[cfg(test)]";
    let (Some(before_marker), Some(after_marker)) = (before.find(marker), after.find(marker))
    else {
        return false;
    };
    before[..before_marker] == after[..after_marker]
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::{ChangeSet, ChangeStatus, ChangedFile, FileEvidence};

    fn evidence(value: &str) -> FileEvidence {
        FileEvidence {
            digest: runner::sha256_digest(value.as_bytes()),
            bytes: value.as_bytes().to_vec(),
            blob: None,
            executable: false,
        }
    }

    fn changes(file: ChangedFile) -> ChangeSet {
        ChangeSet {
            base_revision: "sha256:base".into(),
            patch_digest: "sha256:patch".into(),
            files: vec![file],
        }
    }

    #[test]
    fn green_may_share_the_red_file_but_never_lose_the_red_test() {
        let root = std::env::temp_dir().join(format!("foundry-tdd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let before = "pub fn snapshot() {}\n#[cfg(test)]\nmod tests {}\n";
        let red_after = "pub fn snapshot() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn wal_is_checkpointed() { assert!(false); }\n}\n";
        let red = changes(ChangedFile {
            path: "src/main.rs".into(),
            status: ChangeStatus::Modified,
            before: Some(evidence(before)),
            after: Some(evidence(red_after)),
        });

        // Green implements in the same file and keeps the red test: legal.
        let green = red_after.replace(
            "pub fn snapshot() {}",
            "pub fn snapshot() { checkpoint(); }\nfn checkpoint() {}",
        );
        std::fs::write(root.join("src/main.rs"), &green).unwrap();
        assert!(validate_green_preserves_red(&root, &red).is_ok());

        // Green removes the red test's assertion line: violation.
        let weakened = green.replace("    fn wal_is_checkpointed() { assert!(false); }\n", "");
        std::fs::write(root.join("src/main.rs"), &weakened).unwrap();
        assert!(validate_green_preserves_red(&root, &red).is_err());

        // An untouched dedicated test file is fine; an edited one is not.
        std::fs::write(root.join("src/main.rs"), &green).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        let dedicated_body = "#[test]\nfn proves() { assert!(false); }\n";
        std::fs::write(root.join("tests/red.rs"), dedicated_body).unwrap();
        let dedicated = changes(ChangedFile {
            path: "tests/red.rs".into(),
            status: ChangeStatus::Added,
            before: None,
            after: Some(evidence(dedicated_body)),
        });
        assert!(validate_green_preserves_red(&root, &dedicated).is_ok());
        std::fs::write(root.join("tests/red.rs"), "// gutted\n").unwrap();
        assert!(validate_green_preserves_red(&root, &dedicated).is_err());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn red_phase_rejects_production_edits_and_accepts_test_edits() {
        assert!(
            validate_red_phase_changes(&changes(ChangedFile {
                path: "tests/new_behavior.rs".into(),
                status: ChangeStatus::Added,
                before: None,
                after: Some(evidence("#[test] fn proves_behavior() {}")),
            }))
            .is_ok()
        );
        assert!(
            validate_red_phase_changes(&changes(ChangedFile {
                path: "src/lib.rs".into(),
                status: ChangeStatus::Modified,
                before: Some(evidence("pub fn behavior() {}")),
                after: Some(evidence("pub fn changed_in_red() {}")),
            }))
            .is_err()
        );
        assert!(
            validate_red_phase_changes(&changes(ChangedFile {
                path: "src/lib.rs".into(),
                status: ChangeStatus::Modified,
                before: Some(evidence("pub fn behavior() {}\n#[cfg(test)]\nmod tests {}",)),
                after: Some(evidence(
                    "pub fn behavior() {}\n#[cfg(test)]\nmod tests { #[test] fn new() {} }",
                )),
            }))
            .is_ok()
        );
    }
}
