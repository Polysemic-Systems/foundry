//! Content-addressed storage for large immutable job evidence.

use crate::{FileEvidence, JobResult};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EvidenceStoreError {
    #[error("sqlite error while locating the evidence store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("evidence store I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid evidence digest {0}")]
    InvalidDigest(String),
    #[error("evidence bytes do not match digest {0}")]
    DigestMismatch(String),
    #[error("missing content-addressed evidence object {0}")]
    MissingObject(String),
}

/// Replace inline file bytes with immutable object references for file-backed
/// databases. In-memory graphs retain inline bytes for lightweight tests.
pub fn externalize_job_result(
    conn: &Connection,
    result: &JobResult,
) -> Result<JobResult, EvidenceStoreError> {
    let Some(root) = store_root(conn)? else {
        return Ok(result.clone());
    };
    let mut stored = result.clone();
    visit_evidence_mut(&mut stored, |evidence| externalize(&root, evidence))?;
    Ok(stored)
}

/// Resolve and verify all referenced objects before returning job evidence to
/// a caller. Legacy inline-only records remain supported.
pub fn hydrate_job_result(
    conn: &Connection,
    mut result: JobResult,
) -> Result<JobResult, EvidenceStoreError> {
    let root = store_root(conn)?;
    visit_evidence_mut(&mut result, |evidence| hydrate(root.as_deref(), evidence))?;
    Ok(result)
}

fn store_root(conn: &Connection) -> Result<Option<PathBuf>, EvidenceStoreError> {
    let mut statement = conn.prepare("PRAGMA database_list")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    for row in rows {
        let (name, file) = row?;
        if name == "main" && !file.is_empty() {
            let database = PathBuf::from(file);
            let parent = database.parent().unwrap_or_else(|| Path::new("."));
            return Ok(Some(parent.join("blobs").join("sha256")));
        }
    }
    Ok(None)
}

fn visit_evidence_mut(
    result: &mut JobResult,
    mut visit: impl FnMut(&mut FileEvidence) -> Result<(), EvidenceStoreError>,
) -> Result<(), EvidenceStoreError> {
    if let Some(change_set) = &mut result.change_set {
        for change in &mut change_set.files {
            if let Some(before) = &mut change.before {
                visit(before)?;
            }
            if let Some(after) = &mut change.after {
                visit(after)?;
            }
        }
    }
    Ok(())
}

fn externalize(root: &Path, evidence: &mut FileEvidence) -> Result<(), EvidenceStoreError> {
    let hex = digest_hex(&evidence.digest)?;
    verify_bytes(evidence)?;
    fs::create_dir_all(root).map_err(|source| io_error(root, source))?;
    for path in [
        root.parent().and_then(Path::parent),
        root.parent(),
        Some(root),
    ]
    .into_iter()
    .flatten()
    {
        reject_symlink(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_error(root, source))?;
    }
    let object = root.join(hex);
    if object.exists() {
        reject_symlink(&object)?;
        verify_object(&object, &evidence.digest)?;
    } else {
        let temporary = root.join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
        let mut file =
            fs::File::create(&temporary).map_err(|source| io_error(&temporary, source))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| io_error(&temporary, source))?;
        }
        file.write_all(&evidence.bytes)
            .map_err(|source| io_error(&temporary, source))?;
        file.sync_all()
            .map_err(|source| io_error(&temporary, source))?;
        fs::rename(&temporary, &object).map_err(|source| io_error(&object, source))?;
        sync_directory(root)?;
    }
    evidence.blob = Some(evidence.digest.clone());
    evidence.bytes.clear();
    Ok(())
}

fn hydrate(root: Option<&Path>, evidence: &mut FileEvidence) -> Result<(), EvidenceStoreError> {
    if let Some(reference) = &evidence.blob {
        if reference != &evidence.digest {
            return Err(EvidenceStoreError::InvalidDigest(reference.clone()));
        }
        let root = root.ok_or_else(|| EvidenceStoreError::MissingObject(reference.clone()))?;
        let object = root.join(digest_hex(reference)?);
        if !object.exists() {
            return Err(EvidenceStoreError::MissingObject(reference.clone()));
        }
        evidence.bytes = fs::read(&object).map_err(|source| io_error(&object, source))?;
        // The object reference is a persistence detail. Returning the original
        // content value preserves equality and conflict checks for legacy
        // callers while SQLite remains compact.
        evidence.blob = None;
    }
    verify_bytes(evidence)
}

fn verify_object(path: &Path, expected: &str) -> Result<(), EvidenceStoreError> {
    let bytes = fs::read(path).map_err(|source| io_error(path, source))?;
    let actual = digest(&bytes);
    if actual != expected {
        return Err(EvidenceStoreError::DigestMismatch(expected.to_owned()));
    }
    Ok(())
}

fn verify_bytes(evidence: &FileEvidence) -> Result<(), EvidenceStoreError> {
    if digest(&evidence.bytes) != evidence.digest {
        return Err(EvidenceStoreError::DigestMismatch(evidence.digest.clone()));
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn digest_hex(value: &str) -> Result<&str, EvidenceStoreError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(EvidenceStoreError::InvalidDigest(value.to_owned()));
    };
    if hex.len() != 64 || !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err(EvidenceStoreError::InvalidDigest(value.to_owned()));
    }
    Ok(hex)
}

fn sync_directory(path: &Path) -> Result<(), EvidenceStoreError> {
    let directory = fs::File::open(path).map_err(|source| io_error(path, source))?;
    directory
        .sync_all()
        .map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: std::io::Error) -> EvidenceStoreError {
    EvidenceStoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn reject_symlink(path: &Path) -> Result<(), EvidenceStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() {
        return Err(EvidenceStoreError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other("content store path is a symbolic link"),
        });
    }
    Ok(())
}
