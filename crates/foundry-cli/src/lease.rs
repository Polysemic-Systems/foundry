//! Repository-scoped iteration lease.
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
use std::path::Path;
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
            "{} (pid {}) running `{}` for {}s",
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
                "another iteration holds the repository lease: {info}\n\
                 Wait for it to finish, or inspect it with `foundry lease`."
            ),
            LeaseRefused::Held(None) => write!(
                f,
                "another iteration holds the repository lease (no metadata recorded).\n\
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

/// The lease's current state, as reported by `foundry lease`.
#[derive(Debug)]
pub enum LeaseStatus {
    /// Nobody holds the lock. Metadata, if present, is from the last holder.
    Free(Option<LeaseInfo>),
    Held(Option<LeaseInfo>),
}

/// Acquire the repository iteration lease, refusing if it is already held.
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
}
