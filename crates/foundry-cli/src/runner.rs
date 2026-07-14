use anyhow::{Context, Result, bail};
use foundry_core::{ChangeSet, ChangeStatus, ChangedFile, JobId, JobSpec};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

#[derive(Debug)]
pub struct RunnerOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub cancelled: bool,
    pub change_set: ChangeSet,
}

pub fn run_podman(
    spec: &JobSpec,
    image: &str,
    root: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<RunnerOutput> {
    if spec.command.is_empty() {
        bail!("job command cannot be empty");
    }
    let name = format!("foundry-job-{}", JobId::new().0);
    let args = spec.podman_args(image, &root.to_string_lossy(), &name);
    let before = snapshot(root)?;
    let started = Instant::now();
    let mut child = Command::new("podman")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting Podman job")?;

    let stdout = child.stdout.take().context("capturing job stdout")?;
    let stderr = child.stderr.take().context("capturing job stderr")?;
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let timeout = Duration::from_secs(spec.timeout_seconds);
    let mut timed_out = false;
    let mut was_cancelled = false;

    let status = loop {
        if let Some(status) = child.try_wait().context("polling Podman job")? {
            break status;
        }
        if cancelled.load(Ordering::Relaxed) || started.elapsed() >= timeout {
            was_cancelled = cancelled.load(Ordering::Relaxed);
            timed_out = !was_cancelled;
            let _ = Command::new("podman")
                .args(["stop", "--time", "1", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.kill();
            break child.wait().context("waiting for stopped Podman job")?;
        }
        thread::sleep(Duration::from_millis(50));
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
    let after = snapshot(root)?;
    let change_set = compare_snapshots(&before, &after);
    Ok(RunnerOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        timed_out,
        cancelled: was_cancelled,
        change_set,
    })
}

fn snapshot(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(root).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        !entry.file_type().is_dir() || !matches!(name.as_ref(), ".git" | ".foundry" | "target")
    }) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let bytes = fs::read(entry.path())?;
        files.insert(
            relative.to_string_lossy().into_owned(),
            stable_digest(&bytes),
        );
    }
    Ok(files)
}

fn compare_snapshots(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> ChangeSet {
    let mut files = Vec::new();
    for (path, digest) in after {
        match before.get(path) {
            None => files.push(ChangedFile {
                path: path.clone(),
                status: ChangeStatus::Added,
            }),
            Some(previous) if previous != digest => files.push(ChangedFile {
                path: path.clone(),
                status: ChangeStatus::Modified,
            }),
            _ => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            files.push(ChangedFile {
                path: path.clone(),
                status: ChangeStatus::Deleted,
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut patch = Vec::new();
    for file in &files {
        patch.extend_from_slice(file.path.as_bytes());
        patch.extend_from_slice(format!("{:?}", file.status).as_bytes());
        if let Some(digest) = after.get(&file.path) {
            patch.extend_from_slice(digest.as_bytes());
        }
    }
    let mut base = Vec::new();
    for (path, digest) in before {
        base.extend_from_slice(path.as_bytes());
        base.extend_from_slice(digest.as_bytes());
    }
    ChangeSet {
        base_revision: stable_digest(&base),
        patch_digest: stable_digest(&patch),
        files,
    }
}

fn stable_digest(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn read_all(mut reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_capture_added_modified_and_deleted_files() {
        let root = std::env::temp_dir().join(format!("foundry-runner-test-{}", JobId::new().0));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("modified.txt"), "before").unwrap();
        fs::write(root.join("deleted.txt"), "gone").unwrap();
        let before = snapshot(&root).unwrap();
        fs::write(root.join("modified.txt"), "after").unwrap();
        fs::remove_file(root.join("deleted.txt")).unwrap();
        fs::write(root.join("added.txt"), "new").unwrap();
        let after = snapshot(&root).unwrap();
        let changes = compare_snapshots(&before, &after);
        assert_eq!(changes.files.len(), 3);
        assert!(changes.files.contains(&ChangedFile {
            path: "added.txt".into(),
            status: ChangeStatus::Added
        }));
        assert!(changes.files.contains(&ChangedFile {
            path: "modified.txt".into(),
            status: ChangeStatus::Modified
        }));
        assert!(changes.files.contains(&ChangedFile {
            path: "deleted.txt".into(),
            status: ChangeStatus::Deleted
        }));
        fs::remove_dir_all(root).unwrap();
    }
}
