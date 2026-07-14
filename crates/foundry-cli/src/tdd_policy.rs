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

pub fn validate_green_preserves_red(root: &Path, red: &foundry_core::ChangeSet) -> Result<()> {
    let current = runner::snapshot_workspace(root)
        .context("capturing workspace after the TDD green phase")?;
    let changed = red
        .files
        .iter()
        .filter(|change| current.get(&change.path) != change.after.as_ref())
        .map(|change| change.path.as_str())
        .collect::<Vec<_>>();
    if !changed.is_empty() {
        bail!(
            "TDD green phase changed or removed the test that established the red failure: {}",
            changed.join(", ")
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
