//! Repository-scoped mutation lease.
//!
//! Two concurrent `iterate` processes can select the same task, race the
//! plan file, and stage conflicting evidence. The lease makes the failure
//! mode a clear refusal instead.
//!
//! The lease is an OS advisory lock (`File::try_lock`, `flock` on Linux) on
//! `.foundry/repository.lease`, held for the lifetime of a [`LeaseGuard`]. The
//! kernel releases the lock when the holding process exits for any reason,
//! so a crashed run can never leave a stale lease behind and no break/force
//! command is needed. Owner metadata is written into the file purely for
//! diagnostics; the lock itself is the source of truth.
//!
//! Scope: single host. Advisory locks are not reliable across NFS mounts;
//! a repository shared between hosts needs a different mechanism.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LEASE_FILE: &str = "repository.lease";

/// Diagnostic metadata recorded by the current (or most recent) holder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseInfo {
    pub owner: String,
    pub pid: u32,
    pub operation: String,
    pub acquired_at_epoch_secs: u64,
}

impl LeaseInfo {
    fn age_secs(&self) -> u64 {
        now_epoch_secs().saturating_sub(self.acquired_at_epoch_secs)
    }
}

impl std::fmt::Display for LeaseInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (pid {}), operation `{}`, acquired {}s ago",
            self.owner,
            self.pid,
            self.operation,
            self.age_secs()
        )
    }
}

/// Why the lease could not be acquired.
#[derive(Debug)]
pub enum LeaseRefused {
    /// Another live process holds the lock. Metadata is best-effort: it can
    /// be `None` if the holder died between locking and writing it.
    Held(Option<LeaseInfo>),
    Io(anyhow::Error),
}

impl std::fmt::Display for LeaseRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseRefused::Held(Some(info)) => write!(
                f,
                "another operation holds the repository lease; most recent Foundry metadata: {info}\n\
                 Wait for it to finish, or inspect it with `foundry lease`."
            ),
            LeaseRefused::Held(None) => write!(
                f,
                "another operation holds the repository lease (no metadata recorded).\n\
                 Wait for it to finish, or inspect it with `foundry lease`."
            ),
            LeaseRefused::Io(error) => write!(f, "acquiring repository lease: {error:#}"),
        }
    }
}

impl std::error::Error for LeaseRefused {}

/// Holds the repository lease until dropped. Dropping (or process death,
/// however abrupt) releases the OS lock. The field is never read: its only
/// job is to own the locked file descriptor.
#[derive(Debug)]
pub struct LeaseGuard {
    _file: File,
}

/// Compile-time witness that the repository lease is held for one canonical
/// workspace root. Mutation APIs take this capability instead of a bare path,
/// so callers cannot forget the lease or accidentally use a guard from a
/// different checkout.
#[derive(Debug)]
pub struct RepositoryMutation {
    root: PathBuf,
    _guard: LeaseGuard,
}

impl RepositoryMutation {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn require_path(&self, path: &Path) -> Result<PathBuf> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let mut confined = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    confined.pop();
                }
                _ => confined.push(component.as_os_str()),
            }
        }
        if !confined.starts_with(&self.root) {
            anyhow::bail!(
                "mutation path {} is outside leased repository {}",
                confined.display(),
                self.root.display()
            );
        }

        let relative = confined.strip_prefix(&self.root).with_context(|| {
            format!(
                "deriving repository-relative mutation path {} beneath {}",
                confined.display(),
                self.root.display()
            )
        })?;
        let mut ancestor = self.root.clone();
        for component in relative.components() {
            ancestor.push(component.as_os_str());
            match fs::symlink_metadata(&ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    anyhow::bail!(
                        "mutation path {} traverses symlink {}",
                        confined.display(),
                        ancestor.display()
                    );
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspecting repository path {}", ancestor.display())
                    });
                }
            }
        }

        Ok(confined)
    }
}

/// The lease's current state, as reported by `foundry lease`.
#[derive(Debug)]
pub enum LeaseStatus {
    /// Nobody holds the lock. Metadata, if present, is from the last holder.
    Free(Option<LeaseInfo>),
    Held(Option<LeaseInfo>),
}

/// Acquire the repository mutation lease, refusing if it is already held.
pub fn acquire(
    foundry_dir: &Path,
    owner: &str,
    operation: &str,
) -> Result<LeaseGuard, LeaseRefused> {
    fs::create_dir_all(foundry_dir)
        .with_context(|| format!("creating {foundry_dir:?}"))
        .map_err(LeaseRefused::Io)?;
    let path = foundry_dir.join(LEASE_FILE);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening lease file {path:?}"))
        .map_err(LeaseRefused::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting lease file {path:?}"))
            .map_err(LeaseRefused::Io)?;
    }

    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(LeaseRefused::Held(read_info(&mut file)));
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(LeaseRefused::Io(
                anyhow::Error::new(error).context(format!("locking lease file {path:?}")),
            ));
        }
    }

    let info = LeaseInfo {
        owner: owner.to_string(),
        pid: std::process::id(),
        operation: operation.to_string(),
        acquired_at_epoch_secs: now_epoch_secs(),
    };
    write_info(&mut file, &info)
        .with_context(|| format!("recording lease metadata in {path:?}"))
        .map_err(LeaseRefused::Io)?;

    Ok(LeaseGuard { _file: file })
}

/// Acquire a repository-scoped mutation capability rooted at the canonical
/// workspace path.
pub fn acquire_repository(
    root: &Path,
    owner: &str,
    operation: &str,
) -> Result<RepositoryMutation, LeaseRefused> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving repository root {}", root.display()))
        .map_err(LeaseRefused::Io)?;
    harden_repository_state(&root).map_err(LeaseRefused::Io)?;
    let guard = acquire(&root.join(".foundry"), owner, operation)?;
    tracing::info!(
        repository = %root.display(),
        owner,
        operation,
        "acquired repository mutation lease"
    );
    Ok(RepositoryMutation {
        root,
        _guard: guard,
    })
}

/// Create or migrate Foundry's local state boundary to private permissions.
///
/// Only the state containers and known evidence/database files are changed;
/// attempt workspace contents retain their original executable bits.
pub fn harden_repository_state(root: &Path) -> Result<()> {
    let foundry = root.join(".foundry");
    fs::create_dir_all(&foundry)
        .with_context(|| format!("creating private state directory {}", foundry.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let private_dir = fs::Permissions::from_mode(0o700);
        fs::set_permissions(&foundry, private_dir.clone())
            .with_context(|| format!("restricting {}", foundry.display()))?;
        for relative in [
            "attempts",
            "blobs",
            "blobs/sha256",
            "promotions",
            "snapshots",
            "tdd-baselines",
        ] {
            let path = foundry.join(relative);
            if path.is_dir() {
                fs::set_permissions(&path, private_dir.clone())
                    .with_context(|| format!("restricting {}", path.display()))?;
            }
        }

        let private_file = fs::Permissions::from_mode(0o600);
        for relative in ["db.sqlite", "repository.lease"] {
            let path = foundry.join(relative);
            if path.is_file() {
                fs::set_permissions(&path, private_file.clone())
                    .with_context(|| format!("restricting {}", path.display()))?;
            }
        }
        for relative in ["snapshots", "tdd-baselines"] {
            let directory = foundry.join(relative);
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("reading {}", directory.display()));
                }
            };
            for entry in entries {
                let path = entry?.path();
                if path.is_file() {
                    fs::set_permissions(&path, private_file.clone())
                        .with_context(|| format!("restricting {}", path.display()))?;
                }
            }
        }
    }
    Ok(())
}

/// Report whether the lease is currently held, without taking it for longer
/// than the probe itself.
pub fn inspect(foundry_dir: &Path) -> Result<LeaseStatus> {
    let path = foundry_dir.join(LEASE_FILE);
    let mut file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LeaseStatus::Free(None));
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context(format!("opening lease file {path:?}")));
        }
    };
    match file.try_lock() {
        Ok(()) => {
            let info = read_info(&mut file);
            file.unlock()
                .with_context(|| format!("releasing probe lock on {path:?}"))?;
            Ok(LeaseStatus::Free(info))
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(LeaseStatus::Held(read_info(&mut file))),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(anyhow::Error::new(error).context(format!("probing lease file {path:?}")))
        }
    }
}

/// Identity string for lease metadata: `$USER`, or the pid alone if unset.
pub fn default_owner() -> String {
    std::env::var("USER").unwrap_or_else(|_| format!("pid-{}", std::process::id()))
}

fn write_info(file: &mut File, info: &LeaseInfo) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    serde_json::to_writer_pretty(&mut *file, info)?;
    file.flush()?;
    Ok(())
}

fn read_info(file: &mut File) -> Option<LeaseInfo> {
    let mut contents = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut contents).ok()?;
    serde_json::from_str(&contents).ok()
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("foundry-lease-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn require_path_has_no_production_expect() {
        let source = include_str!("lease.rs");
        let (_, after_signature) = source
            .split_once("    pub fn require_path(&self, path: &Path) -> Result<PathBuf> {")
            .expect("RepositoryMutation::require_path must remain present");
        let (require_path_body, _) = after_signature
            .split_once("\n    }\n}\n\n/// The lease's current state")
            .expect("RepositoryMutation::require_path must remain in its production impl");

        assert!(
            !require_path_body.contains(".expect") && !require_path_body.contains("panic!"),
            "RepositoryMutation::require_path must propagate errors with context, not panic"
        );
    }

    #[test]
    fn acquire_then_second_acquire_is_refused_with_holder_metadata() {
        let dir = scratch_dir();
        let guard = acquire(&dir, "alice", "iterate --tdd").unwrap();

        let refused = acquire(&dir, "bob", "iterate").unwrap_err();
        match refused {
            LeaseRefused::Held(Some(info)) => {
                assert_eq!(info.owner, "alice");
                assert_eq!(info.pid, std::process::id());
                assert_eq!(info.operation, "iterate --tdd");
            }
            other => panic!("expected Held with metadata, got {other:?}"),
        }

        drop(guard);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropping_the_guard_releases_the_lease() {
        let dir = scratch_dir();
        let guard = acquire(&dir, "alice", "iterate").unwrap();
        drop(guard);

        let guard = acquire(&dir, "bob", "iterate").unwrap();
        drop(guard);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn inspect_reports_held_and_free_states() {
        let dir = scratch_dir();

        match inspect(&dir).unwrap() {
            LeaseStatus::Free(None) => {}
            other => panic!("expected Free with no metadata before first use, got {other:?}"),
        }

        let guard = acquire(&dir, "alice", "iterate --tdd").unwrap();
        match inspect(&dir).unwrap() {
            LeaseStatus::Held(Some(info)) => assert_eq!(info.owner, "alice"),
            other => panic!("expected Held, got {other:?}"),
        }

        drop(guard);
        match inspect(&dir).unwrap() {
            LeaseStatus::Free(Some(info)) => {
                assert_eq!(info.owner, "alice", "last holder's metadata is retained");
            }
            other => panic!("expected Free with last-holder metadata, got {other:?}"),
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refusal_message_names_the_holder_and_the_diagnostic_command() {
        let dir = scratch_dir();
        let _guard = acquire(&dir, "alice", "iterate --tdd").unwrap();
        let refused = acquire(&dir, "bob", "iterate").unwrap_err();
        let message = refused.to_string();
        assert!(
            message.contains("alice"),
            "message must name the holder: {message}"
        );
        assert!(
            message.contains("foundry lease"),
            "message must point at the diagnostic command: {message}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lease_survives_metadata_corruption() {
        let dir = scratch_dir();
        fs::write(dir.join(LEASE_FILE), "not json at all").unwrap();

        match inspect(&dir).unwrap() {
            LeaseStatus::Free(None) => {}
            other => panic!("corrupt metadata must degrade to no-metadata, got {other:?}"),
        }

        let guard = acquire(&dir, "alice", "iterate").unwrap();
        drop(guard);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn repository_mutation_is_bound_to_one_canonical_root() {
        let root = scratch_dir();
        let other = scratch_dir();
        let file = root.join("plan.md");
        fs::write(&file, "# Plan").unwrap();
        let other_file = other.join("plan.md");
        fs::write(&other_file, "# Other").unwrap();
        let mutation = acquire_repository(&root, "alice", "test mutation").unwrap();

        assert_eq!(mutation.require_path(&file).unwrap(), file);
        assert!(mutation.require_path(&other_file).is_err());

        drop(mutation);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn propose_new_plan_accepts_a_missing_destination_beneath_the_repository() {
        let root = scratch_dir();
        let plan = root.join("plans/new.plan.md");
        let mutation = acquire_repository(&root, "alice", "propose approval").unwrap();

        assert_eq!(mutation.require_path(&plan).unwrap(), plan);

        drop(mutation);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn propose_new_plan_resolves_a_relative_missing_destination_beneath_the_repository() {
        let root = scratch_dir();
        let relative_plan = Path::new("plans/new-relative.plan.md");
        let mutation = acquire_repository(&root, "alice", "propose approval").unwrap();

        assert_eq!(
            mutation.require_path(relative_plan).unwrap(),
            root.join(relative_plan),
            "relative --plan paths must be resolved against the leased root, not the process CWD"
        );

        drop(mutation);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn propose_new_plan_rejects_a_missing_destination_that_lexically_escapes_the_repository() {
        let root = scratch_dir();
        let plan = root.join("plans/../../outside/new.plan.md");
        let mutation = acquire_repository(&root, "alice", "propose approval").unwrap();

        assert!(mutation.require_path(&plan).is_err());

        drop(mutation);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn propose_new_plan_rejects_a_missing_destination_beneath_a_resolved_symlinked_ancestor() {
        let root = scratch_dir();
        let outside = scratch_dir();
        std::os::unix::fs::symlink(&outside, root.join("plans")).unwrap();
        let plan = root.join("plans/new.plan.md");
        let mutation = acquire_repository(&root, "alice", "propose approval").unwrap();

        assert!(mutation.require_path(&plan).is_err());

        drop(mutation);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn propose_new_plan_rejects_a_resolved_symlinked_ancestor_even_when_it_stays_beneath_root() {
        let root = scratch_dir();
        let real_plans = root.join("real-plans");
        fs::create_dir(&real_plans).unwrap();
        std::os::unix::fs::symlink(&real_plans, root.join("plans")).unwrap();
        let plan = root.join("plans/new.plan.md");
        let mutation = acquire_repository(&root, "alice", "propose approval").unwrap();

        assert!(mutation.require_path(&plan).is_err());

        drop(mutation);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn propose_new_plan_rejects_a_missing_destination_beneath_a_dangling_symlinked_ancestor() {
        let root = scratch_dir();
        std::os::unix::fs::symlink(root.join("missing-target"), root.join("plans")).unwrap();
        let plan = root.join("plans/new.plan.md");
        let mutation = acquire_repository(&root, "alice", "propose approval").unwrap();

        assert!(mutation.require_path(&plan).is_err());

        drop(mutation);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn propose_new_plan_rejects_a_dangling_symlink_destination() {
        let root = scratch_dir();
        let plans = root.join("plans");
        fs::create_dir(&plans).unwrap();
        let plan = plans.join("new.plan.md");
        std::os::unix::fs::symlink(root.join("missing-target.plan.md"), &plan).unwrap();
        let mutation = acquire_repository(&root, "alice", "propose approval").unwrap();

        assert!(mutation.require_path(&plan).is_err());

        drop(mutation);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn repository_acquisition_migrates_sensitive_state_to_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch_dir();
        let foundry = root.join(".foundry");
        let snapshots = foundry.join("snapshots");
        let baselines = foundry.join("tdd-baselines");
        fs::create_dir_all(&snapshots).unwrap();
        fs::create_dir_all(&baselines).unwrap();
        fs::write(foundry.join("db.sqlite"), "database").unwrap();
        fs::write(snapshots.join("old.sqlite"), "snapshot").unwrap();
        fs::write(baselines.join("old.json"), "workspace bytes").unwrap();
        fs::set_permissions(&foundry, fs::Permissions::from_mode(0o755)).unwrap();

        let mutation = acquire_repository(&root, "alice", "permission migration").unwrap();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&foundry), 0o700);
        assert_eq!(mode(&snapshots), 0o700);
        assert_eq!(mode(&baselines), 0o700);
        assert_eq!(mode(&foundry.join("db.sqlite")), 0o600);
        assert_eq!(mode(&snapshots.join("old.sqlite")), 0o600);
        assert_eq!(mode(&baselines.join("old.json")), 0o600);
        assert_eq!(mode(&foundry.join(LEASE_FILE)), 0o600);

        drop(mutation);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lease_metadata_describes_acquisition_age_not_run_duration() {
        let info = LeaseInfo {
            owner: "alice".into(),
            pid: 42,
            operation: "iterate".into(),
            acquired_at_epoch_secs: now_epoch_secs(),
        };
        let rendered = info.to_string();
        assert!(rendered.contains("acquired"));
        assert!(!rendered.contains("running"));
        assert!(!rendered.contains(" for "));
    }
}
