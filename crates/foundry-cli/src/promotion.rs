use crate::runner::{patch_digest, sha256_digest};
use anyhow::{Context, Result, bail};
use foundry_core::{ChangeSet, ChangeStatus, FileEvidence};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
struct PendingChange {
    relative: String,
    target: PathBuf,
    before: Option<FileEvidence>,
    after: Option<FileEvidence>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum JournalState {
    Prepared,
    Committing,
    Committed,
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalEntry {
    path: String,
    before_exists: bool,
    before_digest: Option<String>,
    before_executable: bool,
    after_exists: bool,
    after_digest: Option<String>,
    after_executable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PromotionJournal {
    state: JournalState,
    entries: Vec<JournalEntry>,
}

/// Apply a content-complete staged change after verifying both its immutable
/// evidence and the authoritative workspace's current state.
///
/// The operation is idempotent: files already matching their recorded
/// after-state are accepted, which makes a review retry safe after interruption.
pub fn apply_change_set(root: &Path, change_set: &ChangeSet) -> Result<()> {
    let calculated_patch = patch_digest(&change_set.files);
    if calculated_patch != change_set.patch_digest {
        bail!(
            "corrupt change set: recorded patch digest {}, calculated {calculated_patch}",
            change_set.patch_digest
        );
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving promotion root {}", root.display()))?;
    let _lock = acquire_promotion_lock(&root)?;
    recover_incomplete_promotions(&root)?;
    let mut pending = Vec::new();

    for change in &change_set.files {
        validate_shape(change.status, change.before.as_ref(), change.after.as_ref())?;
        if let Some(evidence) = &change.before {
            validate_blob(evidence, &change.path, "before")?;
        }
        if let Some(evidence) = &change.after {
            validate_blob(evidence, &change.path, "after")?;
        }

        let target = safe_target(&root, &change.path)?;
        let current = read_evidence(&target)?;
        if current.as_ref() == change.after.as_ref() {
            continue;
        }
        if current.as_ref() != change.before.as_ref() {
            bail!(
                "promotion conflict for {}: authoritative workspace matches neither recorded before nor after state",
                change.path
            );
        }
        pending.push(PendingChange {
            relative: change.path.clone(),
            target,
            before: current,
            after: change.after.clone(),
        });
    }

    if pending.is_empty() {
        return Ok(());
    }
    let (transaction, mut journal) = prepare_transaction(&root, &pending)?;
    journal.state = JournalState::Committing;
    persist_journal(&transaction, &journal)?;

    let commit = commit_transaction(&root, &transaction, &journal);
    if let Err(error) = commit {
        rollback_transaction(&root, &transaction, &journal)
            .context("rolling back failed promotion")?;
        return Err(error);
    }
    journal.state = JournalState::Committed;
    persist_journal(&transaction, &journal)?;
    fs::remove_dir_all(&transaction)
        .with_context(|| format!("removing promotion journal {}", transaction.display()))?;
    sync_directory(&root.join(".foundry/promotions"))?;
    Ok(())
}

/// Restore any transaction that was interrupted before its committed marker.
/// Committed transactions are merely cleaned up, making approval retries safe.
pub fn recover_incomplete_promotions(root: &Path) -> Result<()> {
    let transactions = promotion_root(root)?;
    let entries = match fs::read_dir(&transactions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("reading promotion journals"),
    };
    for entry in entries {
        let transaction = entry?.path();
        if !transaction.is_dir() {
            continue;
        }
        let manifest = transaction.join("journal.json");
        if !manifest.exists() {
            // A crash during preparation cannot have touched authoritative
            // files because the committing state is persisted first.
            fs::remove_dir_all(&transaction)?;
            continue;
        }
        let journal: PromotionJournal = serde_json::from_slice(
            &fs::read(&manifest).with_context(|| format!("reading {}", manifest.display()))?,
        )?;
        if journal.state != JournalState::Committed {
            rollback_transaction(root, &transaction, &journal)?;
        } else {
            fs::remove_dir_all(&transaction)?;
        }
    }
    sync_directory(&transactions)?;
    Ok(())
}

fn prepare_transaction(
    root: &Path,
    pending: &[PendingChange],
) -> Result<(PathBuf, PromotionJournal)> {
    let transactions = promotion_root(root)?;
    fs::create_dir_all(&transactions)?;
    let transaction = transactions.join(uuid::Uuid::new_v4().simple().to_string());
    fs::create_dir_all(transaction.join("before"))?;
    fs::create_dir_all(transaction.join("after"))?;
    let mut entries = Vec::with_capacity(pending.len());
    for (index, change) in pending.iter().enumerate() {
        if let Some(before) = &change.before {
            write_staged_file(&transaction.join("before").join(index.to_string()), before)?;
        }
        if let Some(after) = &change.after {
            write_staged_file(&transaction.join("after").join(index.to_string()), after)?;
        }
        entries.push(JournalEntry {
            path: change.relative.clone(),
            before_exists: change.before.is_some(),
            before_digest: change.before.as_ref().map(|value| value.digest.clone()),
            before_executable: change.before.as_ref().is_some_and(|value| value.executable),
            after_exists: change.after.is_some(),
            after_digest: change.after.as_ref().map(|value| value.digest.clone()),
            after_executable: change.after.as_ref().is_some_and(|value| value.executable),
        });
        debug_assert_eq!(safe_target(root, &change.relative).unwrap(), change.target);
    }
    let journal = PromotionJournal {
        state: JournalState::Prepared,
        entries,
    };
    persist_journal(&transaction, &journal)?;
    sync_directory(&transactions)?;
    Ok((transaction, journal))
}

fn commit_transaction(root: &Path, transaction: &Path, journal: &PromotionJournal) -> Result<()> {
    for (index, entry) in journal.entries.iter().enumerate() {
        let target = safe_target(root, &entry.path)?;
        if entry.after_exists {
            install_staged_file(
                &transaction.join("after").join(index.to_string()),
                &target,
                entry.after_executable,
            )?;
        } else if target.exists() {
            fs::remove_file(&target)
                .with_context(|| format!("deleting promoted file {}", target.display()))?;
            sync_parent(&target)?;
        }
    }
    for entry in &journal.entries {
        let target = safe_target(root, &entry.path)?;
        if target.exists() != entry.after_exists {
            bail!("promoted file {} failed existence verification", entry.path);
        }
        if let Some(expected) = &entry.after_digest {
            let actual = read_evidence(&target)?.context("promoted file disappeared")?;
            if &actual.digest != expected || actual.executable != entry.after_executable {
                bail!("promoted file {} failed content verification", entry.path);
            }
        }
    }
    Ok(())
}

fn rollback_transaction(root: &Path, transaction: &Path, journal: &PromotionJournal) -> Result<()> {
    for (index, entry) in journal.entries.iter().enumerate() {
        let target = safe_target(root, &entry.path)?;
        if entry.before_exists {
            install_staged_file(
                &transaction.join("before").join(index.to_string()),
                &target,
                entry.before_executable,
            )?;
        } else if target.exists() {
            fs::remove_file(&target)?;
            sync_parent(&target)?;
        }
    }
    for entry in &journal.entries {
        let target = safe_target(root, &entry.path)?;
        if target.exists() != entry.before_exists {
            bail!(
                "recovered file {} failed existence verification",
                entry.path
            );
        }
        if let Some(expected) = &entry.before_digest {
            let actual = read_evidence(&target)?.context("recovered file disappeared")?;
            if &actual.digest != expected || actual.executable != entry.before_executable {
                bail!("recovered file {} failed content verification", entry.path);
            }
        }
    }
    fs::remove_dir_all(transaction)
        .with_context(|| format!("removing recovered journal {}", transaction.display()))?;
    Ok(())
}

fn persist_journal(transaction: &Path, journal: &PromotionJournal) -> Result<()> {
    let path = transaction.join("journal.json");
    let temporary = transaction.join("journal.tmp");
    let bytes = serde_json::to_vec(journal)?;
    let mut file = fs::File::create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, &path)?;
    sync_directory(transaction)
}

fn write_staged_file(path: &Path, evidence: &FileEvidence) -> Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(&evidence.bytes)?;
    file.sync_all()?;
    set_executable(path, evidence.executable)
}

fn install_staged_file(source: &Path, target: &Path, executable: bool) -> Result<()> {
    let bytes = fs::read(source)
        .with_context(|| format!("reading staged promotion {}", source.display()))?;
    let evidence = FileEvidence {
        digest: sha256_digest(&bytes),
        bytes,
        blob: None,
        executable,
    };
    write_evidence(target, &evidence)?;
    sync_parent(target)
}

fn sync_parent(path: &Path) -> Result<()> {
    sync_directory(path.parent().context("promotion target has no parent")?)
}

fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("opening directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing directory {}", path.display()))
}

fn promotion_root(root: &Path) -> Result<PathBuf> {
    safe_target(root, ".foundry/promotions")
}

fn acquire_promotion_lock(root: &Path) -> Result<fs::File> {
    let foundry = safe_target(root, ".foundry")?;
    fs::create_dir_all(&foundry)?;
    let lock_path = safe_target(root, ".foundry/promotion.lock")?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening promotion lock {}", lock_path.display()))?;
    lock.try_lock()
        .with_context(|| "another approval is currently promoting changes")?;
    Ok(lock)
}

fn set_executable(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, executable);
    Ok(())
}

fn validate_shape(
    status: ChangeStatus,
    before: Option<&FileEvidence>,
    after: Option<&FileEvidence>,
) -> Result<()> {
    let valid = matches!(
        (status, before.is_some(), after.is_some()),
        (ChangeStatus::Added, false, true)
            | (ChangeStatus::Modified, true, true)
            | (ChangeStatus::Deleted, true, false)
    );
    if !valid {
        bail!("staged change set is not content-complete for {status:?}");
    }
    Ok(())
}

fn validate_blob(evidence: &FileEvidence, path: &str, side: &str) -> Result<()> {
    let actual = sha256_digest(&evidence.bytes);
    if actual != evidence.digest {
        bail!(
            "corrupt {side} evidence for {path}: recorded {}, calculated {actual}",
            evidence.digest
        );
    }
    Ok(())
}

fn safe_target(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("change path must be a normalized workspace-relative path: {relative:?}");
    }

    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        if let Ok(metadata) = fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            bail!(
                "promotion path crosses a symbolic link: {}",
                current.display()
            );
        }
    }
    Ok(root.join(relative))
}

fn read_evidence(path: &Path) -> Result<Option<FileEvidence>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if !metadata.is_file() {
        bail!("promotion target is not a regular file: {}", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(Some(FileEvidence {
        digest: sha256_digest(&bytes),
        bytes,
        blob: None,
        executable,
    }))
}

fn write_evidence(path: &Path, evidence: &FileEvidence) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("promoted file has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating promoted directory {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".foundry-promote-{}",
        uuid::Uuid::new_v4().simple()
    ));
    fs::write(&temporary, &evidence.bytes)
        .with_context(|| format!("writing staged promotion {}", temporary.display()))?;
    set_executable(&temporary, evidence.executable)?;
    fs::rename(&temporary, path).with_context(|| format!("promoting file {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::{ChangeSet, ChangedFile};

    fn evidence(value: &str) -> FileEvidence {
        FileEvidence {
            digest: sha256_digest(value.as_bytes()),
            bytes: value.as_bytes().to_vec(),
            blob: None,
            executable: false,
        }
    }

    fn change_set(file: ChangedFile) -> ChangeSet {
        let files = vec![file];
        ChangeSet {
            base_revision: "sha256:base".into(),
            patch_digest: patch_digest(&files),
            files,
        }
    }

    #[test]
    fn promotion_is_conflict_checked_and_idempotent() {
        let root = std::env::temp_dir().join(format!("foundry-promotion-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file.txt"), "before").unwrap();
        let changes = change_set(ChangedFile {
            path: "file.txt".into(),
            status: ChangeStatus::Modified,
            before: Some(evidence("before")),
            after: Some(evidence("after")),
        });

        apply_change_set(&root, &changes).unwrap();
        apply_change_set(&root, &changes).unwrap();
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), "after");

        fs::write(root.join("file.txt"), "human edit").unwrap();
        assert!(apply_change_set(&root, &changes).is_err());
        assert_eq!(
            fs::read_to_string(root.join("file.txt")).unwrap(),
            "human edit"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn promotion_rejects_tampered_evidence_before_editing() {
        let root = std::env::temp_dir().join(format!("foundry-promotion-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let mut after = evidence("after");
        after.bytes = b"tampered".to_vec();
        let changes = change_set(ChangedFile {
            path: "file.txt".into(),
            status: ChangeStatus::Added,
            before: None,
            after: Some(after),
        });

        assert!(apply_change_set(&root, &changes).is_err());
        assert!(!root.join("file.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_promotion_is_rolled_back_from_durable_journal() {
        let root = std::env::temp_dir().join(format!("foundry-promotion-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join(".foundry/promotions/interrupted/before")).unwrap();
        fs::create_dir_all(root.join(".foundry/promotions/interrupted/after")).unwrap();
        fs::write(root.join("file.txt"), "after-partial-commit").unwrap();
        fs::write(
            root.join(".foundry/promotions/interrupted/before/0"),
            "before",
        )
        .unwrap();
        let journal = PromotionJournal {
            state: JournalState::Committing,
            entries: vec![JournalEntry {
                path: "file.txt".into(),
                before_exists: true,
                before_digest: Some(sha256_digest(b"before")),
                before_executable: false,
                after_exists: true,
                after_digest: Some(sha256_digest(b"after-partial-commit")),
                after_executable: false,
            }],
        };
        persist_journal(&root.join(".foundry/promotions/interrupted"), &journal).unwrap();

        recover_incomplete_promotions(&root).unwrap();

        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), "before");
        assert!(!root.join(".foundry/promotions/interrupted").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
