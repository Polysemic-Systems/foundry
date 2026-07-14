use crate::runner::sha256_digest;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Prepare a deterministic isolated workspace for one task. An existing
/// directory is deliberately reused so an interruption before durable job
/// creation does not discard editor-agent work.
pub fn prepare(root: &Path, task_key: &str) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving authoritative workspace {}", root.display()))?;
    let attempts = root.join(".foundry").join("attempts");
    fs::create_dir_all(&attempts)
        .with_context(|| format!("creating attempt directory {}", attempts.display()))?;
    let key = sha256_digest(task_key.as_bytes())
        .strip_prefix("sha256:")
        .unwrap_or("invalid")
        .to_owned();
    let destination = attempts.join(key);
    if destination.is_dir() {
        return Ok(destination);
    }

    let temporary = attempts.join(format!(".creating-{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir(&temporary)
        .with_context(|| format!("creating temporary attempt {}", temporary.display()))?;
    if let Err(error) = copy_workspace(&root, &temporary) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, &destination).with_context(|| {
        format!(
            "publishing isolated attempt {} as {}",
            temporary.display(),
            destination.display()
        )
    })?;
    Ok(destination)
}

pub fn discard(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing attempt {}", path.display())),
    }
}

fn copy_workspace(root: &Path, destination: &Path) -> Result<()> {
    for entry in WalkDir::new(root).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        !entry.file_type().is_dir() || !matches!(name.as_ref(), ".git" | ".foundry" | "target")
    }) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(root)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_symlink() {
            bail!(
                "isolated attempts do not follow symbolic links: {}",
                relative.display()
            );
        }
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {} to isolated attempt", relative.display()))?;
            fs::set_permissions(&target, entry.metadata()?.permissions())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_is_isolated_reusable_and_excludes_foundry_state() {
        let root = std::env::temp_dir().join(format!("foundry-attempt-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join(".foundry")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("source.txt"), "authoritative").unwrap();
        fs::write(root.join(".foundry/secret"), "state").unwrap();
        fs::write(root.join("target/build"), "cache").unwrap();

        let attempt = prepare(&root, "task-one").unwrap();
        fs::write(attempt.join("source.txt"), "agent edit").unwrap();
        assert_eq!(
            fs::read_to_string(root.join("source.txt")).unwrap(),
            "authoritative"
        );
        assert!(!attempt.join(".foundry/secret").exists());
        assert!(!attempt.join("target/build").exists());
        assert_eq!(prepare(&root, "task-one").unwrap(), attempt);
        assert_eq!(
            fs::read_to_string(attempt.join("source.txt")).unwrap(),
            "agent edit"
        );

        discard(&attempt).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
