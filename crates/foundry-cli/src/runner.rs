use crate::manifest;
use anyhow::{Context, Result, bail};
use foundry_core::{ChangeSet, ChangeStatus, ChangedFile, FileEvidence, JobId, JobSpec};
use sha2::{Digest, Sha256};
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

/// Maximum bytes captured from a job's stdout/stderr before truncation.
const MAX_CAPTURE_BYTES: usize = 10 * 1024 * 1024;
const _: () = assert!(
    MAX_CAPTURE_BYTES > 0,
    "the runner must define a named, non-zero byte limit"
);

/// Content-complete evidence for every relevant regular file in a workspace.
pub type WorkspaceSnapshot = BTreeMap<String, FileEvidence>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCapture {
    pub text: String,
    pub truncated: bool,
    pub dropped_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct RunnerOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stdout_truncated: bool,
    pub stdout_dropped_bytes: usize,
    pub stderr: String,
    pub stderr_truncated: bool,
    pub stderr_dropped_bytes: usize,
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
    let mut args = spec.podman_args(image, &root.to_string_lossy(), &name);
    add_cargo_cache_mount(spec, &mut args)?;
    add_path_dependency_mounts(spec, root, &mut args)?;
    let before = snapshot_workspace(root)?;
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
    let stdout_reader = thread::spawn(move || read_all_limited(stdout, MAX_CAPTURE_BYTES));
    let stderr_reader = thread::spawn(move || read_all_limited(stderr, MAX_CAPTURE_BYTES));
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

    let stdout_cap = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
    let stderr_cap = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
    let after = snapshot_workspace(root)?;
    let change_set = compare_snapshots(&before, &after);
    Ok(RunnerOutput {
        exit_code: status.code(),
        stdout: stdout_cap.text,
        stdout_truncated: stdout_cap.truncated,
        stdout_dropped_bytes: stdout_cap.dropped_bytes,
        stderr: stderr_cap.text,
        stderr_truncated: stderr_cap.truncated,
        stderr_dropped_bytes: stderr_cap.dropped_bytes,
        duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        timed_out,
        cancelled: was_cancelled,
        change_set,
    })
}

/// Workspace `path = "../<sibling>/…"` dependencies live outside the mounted
/// workspace, so a cargo job inside the container cannot see them. For an
/// editor-agent attempt, derive the allowlist from the authoritative workspace
/// manifest, never the agent-writable copy.
fn add_path_dependency_mounts(spec: &JobSpec, root: &Path, args: &mut Vec<String>) -> Result<()> {
    if spec.command.first().map(String::as_str) != Some("cargo") {
        return Ok(());
    }
    let manifest_root = authoritative_manifest_root(root);
    let Some(parent) = manifest_root.parent() else {
        return Ok(());
    };
    let siblings = manifest::sibling_path_dependencies(&manifest_root)?;
    if siblings.is_empty() {
        return Ok(());
    }
    let image_index = args
        .len()
        .checked_sub(spec.command.len() + 1)
        .context("invalid Podman argument layout")?;
    let mut mounts = Vec::new();
    for name in siblings {
        let host = parent.join(&name);
        if !host.is_dir() {
            bail!(
                "authoritative workspace references path dependency ../{name} but {} does not exist",
                host.display()
            );
        }
        let host = host.canonicalize().with_context(|| {
            format!("resolving authoritative path dependency {}", host.display())
        })?;
        mounts.extend(["--volume".into(), format!("{}:/{name}:ro", host.display())]);
    }
    args.splice(image_index..image_index, mounts);
    Ok(())
}

fn authoritative_manifest_root(root: &Path) -> std::path::PathBuf {
    let Some(attempts) = root.parent() else {
        return root.to_path_buf();
    };
    let Some(foundry) = attempts.parent() else {
        return root.to_path_buf();
    };
    if attempts.file_name() == Some(std::ffi::OsStr::new("attempts"))
        && foundry.file_name() == Some(std::ffi::OsStr::new(".foundry"))
        && let Some(authoritative) = foundry.parent()
    {
        return authoritative.to_path_buf();
    }
    root.to_path_buf()
}

fn add_cargo_cache_mount(spec: &JobSpec, args: &mut Vec<String>) -> Result<()> {
    if spec.command.first().map(String::as_str) != Some("cargo") {
        return Ok(());
    }
    let cargo_home = std::env::var_os("FOUNDRY_CARGO_HOME")
        .or_else(|| std::env::var_os("CARGO_HOME"))
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .map(|home| home.join(".cargo"))
        })
        .context("cannot locate Cargo home for offline sandbox")?;
    if !cargo_home.join("registry").is_dir() {
        bail!(
            "Cargo registry cache is missing at {}; run `cargo fetch --locked` first or set FOUNDRY_CARGO_HOME",
            cargo_home.display()
        );
    }

    // Insert runner options immediately before IMAGE and COMMAND.
    let image_index = args
        .len()
        .checked_sub(spec.command.len() + 1)
        .context("invalid Podman argument layout")?;
    // Mount only the caches, never the host cargo config: config.toml names
    // host policy (sccache wrappers, clang/mold linkers, custom profiles)
    // that must not leak into the hermetic verification container. The empty
    // wrapper overrides are belt and braces for configs found elsewhere.
    let mut mounts = vec![
        "--volume".into(),
        format!(
            "{}:/foundry-cargo/registry:ro",
            cargo_home.join("registry").display()
        ),
        "--env".into(),
        "CARGO_HOME=/foundry-cargo".into(),
        "--env".into(),
        "CARGO_NET_OFFLINE=true".into(),
        "--env".into(),
        "RUSTC_WRAPPER=".into(),
        "--env".into(),
        "RUSTC_WORKSPACE_WRAPPER=".into(),
    ];
    let git_cache = cargo_home.join("git");
    if git_cache.is_dir() {
        mounts.extend([
            "--volume".into(),
            format!("{}:/foundry-cargo/git:ro", git_cache.display()),
        ]);
    }
    args.splice(image_index..image_index, mounts);
    Ok(())
}

pub fn snapshot_workspace(root: &Path) -> Result<WorkspaceSnapshot> {
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(root).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        !entry.file_type().is_dir()
            || !matches!(
                name.as_ref(),
                ".git" | ".foundry" | ".foundry-agent-tmp" | "target"
            )
    }) {
        let entry = entry?;
        if entry.file_type().is_symlink() {
            let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
            bail!(
                "workspace evidence refuses symbolic link {}",
                relative.display()
            );
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let bytes = fs::read(entry.path())?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            entry.metadata()?.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        files.insert(
            relative.to_string_lossy().into_owned(),
            FileEvidence {
                digest: sha256_digest(&bytes),
                bytes,
                blob: None,
                executable,
            },
        );
    }
    Ok(files)
}

pub fn compare_snapshots(before: &WorkspaceSnapshot, after: &WorkspaceSnapshot) -> ChangeSet {
    let mut files = Vec::new();
    for (path, evidence) in after {
        match before.get(path) {
            None => files.push(ChangedFile {
                path: path.clone(),
                status: ChangeStatus::Added,
                before: None,
                after: Some(evidence.clone()),
            }),
            Some(previous) if previous != evidence => files.push(ChangedFile {
                path: path.clone(),
                status: ChangeStatus::Modified,
                before: Some(previous.clone()),
                after: Some(evidence.clone()),
            }),
            _ => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            files.push(ChangedFile {
                path: path.clone(),
                status: ChangeStatus::Deleted,
                before: before.get(path).cloned(),
                after: None,
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut base = Vec::new();
    for (path, evidence) in before {
        base.extend_from_slice(path.as_bytes());
        base.extend_from_slice(evidence.digest.as_bytes());
    }
    ChangeSet {
        base_revision: sha256_digest(&base),
        patch_digest: patch_digest(&files),
        files,
    }
}

pub fn patch_digest(files: &[ChangedFile]) -> String {
    let mut patch = Vec::new();
    for file in files {
        patch.extend_from_slice(file.path.as_bytes());
        patch.extend_from_slice(format!("{:?}", file.status).as_bytes());
        if let Some(evidence) = &file.before {
            patch.extend_from_slice(evidence.digest.as_bytes());
        }
        if let Some(evidence) = &file.after {
            patch.extend_from_slice(evidence.digest.as_bytes());
        }
    }
    sha256_digest(&patch)
}

/// Compare a previously captured baseline with the workspace as it exists now.
pub fn changes_since(before: &WorkspaceSnapshot, root: &Path) -> Result<ChangeSet> {
    let after = snapshot_workspace(root)?;
    Ok(compare_snapshots(before, &after))
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn read_all_limited(mut reader: impl Read, limit: usize) -> Result<StreamCapture> {
    const CHUNK: usize = 8192;
    let mut kept = Vec::with_capacity(limit.min(CHUNK));
    let mut total: usize = 0;
    let mut buf = [0u8; CHUNK];

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n;
        if kept.len() < limit {
            let take = n.min(limit - kept.len());
            kept.extend_from_slice(&buf[..take]);
        }
    }

    if total > limit {
        let prefix_end = truncate_to_char_boundary(&kept, limit);
        let dropped = total - prefix_end;
        Ok(StreamCapture {
            text: String::from_utf8_lossy(&kept[..prefix_end]).into_owned(),
            truncated: true,
            dropped_bytes: dropped,
        })
    } else {
        Ok(StreamCapture {
            text: String::from_utf8_lossy(&kept).into_owned(),
            truncated: false,
            dropped_bytes: 0,
        })
    }
}

fn truncate_to_char_boundary(bytes: &[u8], limit: usize) -> usize {
    let mut end = bytes.len().min(limit);
    while end > 0 {
        let start = utf8_char_start(bytes, end - 1);
        let expected = utf8_char_len(bytes[start]);
        if end - start == expected {
            return end;
        }
        end = start;
    }
    0
}

fn utf8_char_start(bytes: &[u8], mut idx: usize) -> usize {
    while idx > 0 && (bytes[idx] & 0b1100_0000) == 0b1000_0000 {
        idx -= 1;
    }
    idx
}

fn utf8_char_len(leading: u8) -> usize {
    match leading.leading_ones() as usize {
        0 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        _ => 1, // invalid leading byte; treat as single byte
    }
}

pub fn ensure_container_toolchain(spec: &mut JobSpec, image: &str, root: &Path) -> Result<()> {
    let rust_command = spec
        .command
        .first()
        .is_some_and(|program| matches!(program.as_str(), "cargo" | "rustc" | "rustup" | "just"));
    if !rust_command || spec.environment.contains_key("RUSTUP_TOOLCHAIN") {
        return Ok(());
    }

    let discovery = JobSpec {
        command: vec!["rustup".into(), "toolchain".into(), "list".into()],
        // Avoid the repository's rust-toolchain.toml while discovering what
        // the image already has available offline.
        working_directory: "/tmp".into(),
        environment: Default::default(),
        timeout_seconds: 60,
        cpu_limit: None,
        memory_limit_bytes: None,
        network_enabled: false,
    };
    let output = run_podman(&discovery, image, root, Arc::new(AtomicBool::new(false)))?;
    if output.exit_code != Some(0) {
        bail!(
            "cannot discover container Rust toolchain: {}",
            output.stderr.trim()
        );
    }
    let toolchain = output
        .stdout
        .lines()
        .find(|line| line.contains("default"))
        .or_else(|| output.stdout.lines().next())
        .and_then(|line| line.split_whitespace().next())
        .context("container image has no installed rustup toolchain")?;
    spec.environment
        .insert("RUSTUP_TOOLCHAIN".into(), toolchain.to_owned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_dependency_siblings_are_mounted_read_only() {
        let parent = std::env::temp_dir().join(format!("foundry-mounts-{}", JobId::new().0));
        let root = parent.join("app");
        fs::create_dir_all(root.join("crates")).unwrap();
        fs::create_dir_all(parent.join("sibling/crates/dep")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "# path = \"../commented-out\"\n\
             [package]\n\
             name = \"fixture\"\n\
             version = \"0.0.0\"\n\
             [package.metadata.fixture]\n\
             path = \"../metadata-only\"\n\
             [workspace.dependencies]\n\
             dep = { path = \"../sibling/crates/dep\" }\n\
             dep2 = { package = \"x\", path = \"../sibling/crates/other\" }\n\
             local = { path = \"crates/local\" }\n",
        )
        .unwrap();

        let spec = JobSpec {
            command: vec!["cargo".into(), "test".into()],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            timeout_seconds: 60,
            cpu_limit: None,
            memory_limit_bytes: None,
            network_enabled: false,
        };
        let mut args: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "img".into(),
            "cargo".into(),
            "test".into(),
        ];
        add_path_dependency_mounts(&spec, &root, &mut args).unwrap();
        let volume = format!("{}:/sibling:ro", parent.join("sibling").display());
        assert_eq!(
            args,
            vec![
                "run".to_string(),
                "--rm".into(),
                "--volume".into(),
                volume,
                "img".into(),
                "cargo".into(),
                "test".into(),
            ],
            "one read-only mount per referenced sibling, before the image"
        );

        // A referenced sibling that does not exist is an error, not a
        // confusing in-container failure.
        fs::write(
            root.join("Cargo.toml"),
            "[dependencies]\ndep = { path = \"../missing/crates/dep\" }\n",
        )
        .unwrap();
        let mut args: Vec<String> = vec!["run".into(), "img".into(), "cargo".into(), "test".into()];
        assert!(add_path_dependency_mounts(&spec, &root, &mut args).is_err());

        // Non-cargo commands are untouched.
        let spec = JobSpec {
            command: vec!["ls".into()],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            timeout_seconds: 60,
            cpu_limit: None,
            memory_limit_bytes: None,
            network_enabled: false,
        };
        let mut args: Vec<String> = vec!["run".into(), "img".into(), "ls".into()];
        add_path_dependency_mounts(&spec, &root, &mut args).unwrap();
        assert_eq!(args, vec!["run", "img", "ls"]);

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn agent_attempt_cannot_expand_the_authoritative_dependency_mount_allowlist() {
        let parent =
            std::env::temp_dir().join(format!("foundry-attempt-mounts-{}", JobId::new().0));
        let authoritative = parent.join("app");
        let attempts = authoritative.join(".foundry/attempts");
        let attempt = attempts.join("task-hash");
        let victim = attempts.join("victim-task-hash");
        fs::create_dir_all(&attempt).unwrap();
        fs::create_dir_all(&victim).unwrap();
        fs::create_dir_all(parent.join("sibling/crates/dep")).unwrap();
        fs::write(
            authoritative.join("Cargo.toml"),
            "[workspace.dependencies]\ndep = { path = \"../sibling/crates/dep\" }\n",
        )
        .unwrap();
        fs::write(
            attempt.join("Cargo.toml"),
            "[dependencies]\nstolen = { path = \"../victim-task-hash/crate\" }\n",
        )
        .unwrap();
        let spec = JobSpec {
            command: vec!["cargo".into(), "test".into()],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            timeout_seconds: 60,
            cpu_limit: None,
            memory_limit_bytes: None,
            network_enabled: false,
        };
        let mut args: Vec<String> = vec!["run".into(), "img".into(), "cargo".into(), "test".into()];

        add_path_dependency_mounts(&spec, &attempt, &mut args).unwrap();

        let rendered = args.join(" ");
        assert!(rendered.contains(":/sibling:ro"));
        assert!(
            !rendered.contains("victim-task-hash"),
            "agent-edited manifests must not mount another attempt: {rendered}"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn snapshots_capture_added_modified_and_deleted_files() {
        let root = std::env::temp_dir().join(format!("foundry-runner-test-{}", JobId::new().0));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("modified.txt"), "before").unwrap();
        fs::write(root.join("deleted.txt"), "gone").unwrap();
        let before = snapshot_workspace(&root).unwrap();
        fs::write(root.join("modified.txt"), "after").unwrap();
        fs::remove_file(root.join("deleted.txt")).unwrap();
        fs::write(root.join("added.txt"), "new").unwrap();
        let after = snapshot_workspace(&root).unwrap();
        let changes = compare_snapshots(&before, &after);
        assert_eq!(changes.files.len(), 3);
        assert!(changes.files.contains(&ChangedFile {
            path: "added.txt".into(),
            status: ChangeStatus::Added,
            before: None,
            after: after.get("added.txt").cloned(),
        }));
        assert!(changes.files.contains(&ChangedFile {
            path: "modified.txt".into(),
            status: ChangeStatus::Modified,
            before: before.get("modified.txt").cloned(),
            after: after.get("modified.txt").cloned(),
        }));
        assert!(changes.files.contains(&ChangedFile {
            path: "deleted.txt".into(),
            status: ChangeStatus::Deleted,
            before: before.get("deleted.txt").cloned(),
            after: None,
        }));
        assert!(changes.base_revision.starts_with("sha256:"));
        assert!(changes.patch_digest.starts_with("sha256:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_snapshot_rejects_symlinks_instead_of_silently_omitting_them() {
        let parent =
            std::env::temp_dir().join(format!("foundry-runner-symlink-{}", JobId::new().0));
        let root = parent.join("workspace");
        fs::create_dir_all(&root).unwrap();
        fs::write(parent.join("outside.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(parent.join("outside.txt"), root.join("escape")).unwrap();

        let error = snapshot_workspace(&root).unwrap_err().to_string();
        assert!(
            error.contains("symbolic link"),
            "symlink refusal must be explicit evidence, not omission: {error}"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    // Capture-limit tests for the runner's stdout/stderr readers.
    // These exercise `read_all_limited` and `MAX_CAPTURE_BYTES`.  The helper
    // must return structured per-stream evidence (text, truncated flag, and
    // dropped-byte count) rather than splicing a human-readable marker into
    // the captured text.

    use std::io::Cursor;

    #[test]
    fn capture_stream_within_limit_is_unmodified() {
        let input = "hello, world\n";
        let cap: StreamCapture = read_all_limited(Cursor::new(input), 1024).unwrap();
        assert_eq!(cap.text, input);
        assert!(
            !cap.truncated,
            "streams under the limit must not be flagged truncated"
        );
        assert_eq!(
            cap.dropped_bytes, 0,
            "streams under the limit must report zero dropped bytes"
        );
        assert!(
            !cap.text.contains("[output truncated"),
            "the captured text must not contain a truncation marker"
        );
    }

    #[test]
    fn capture_stream_over_limit_records_structured_truncation_not_text_marker() {
        let limit = 16;
        let input = "x".repeat(100);
        let cap: StreamCapture = read_all_limited(Cursor::new(&input), limit).unwrap();
        assert_eq!(
            cap.text,
            "x".repeat(limit),
            "only the prefix up to the limit is kept"
        );
        assert!(cap.truncated, "the stream must be flagged as truncated");
        assert_eq!(
            cap.dropped_bytes,
            input.len() - limit,
            "the dropped-byte count must record exactly what was discarded"
        );
        assert!(
            !cap.text.contains("[output truncated"),
            "the captured text must not contain a truncation marker"
        );
    }

    #[test]
    fn capture_stream_truncation_respects_utf8_boundaries() {
        // Each Greek letter is two UTF-8 bytes, for a total of 14 bytes.
        let input = "αβγδεηθ";
        let limit = 3; // falls in the middle of the second character (β)
        let cap: StreamCapture = read_all_limited(Cursor::new(input), limit).unwrap();

        assert!(
            input.is_char_boundary(cap.text.len()),
            "truncation must not split a UTF-8 codepoint"
        );
        assert_eq!(cap.text, "α");
        assert!(cap.truncated);
        assert_eq!(
            cap.dropped_bytes,
            input.len() - cap.text.len(),
            "dropped count must reflect the actual kept bytes"
        );
        assert!(
            !cap.text.contains("[output truncated"),
            "the captured text must not contain a truncation marker"
        );
    }

    #[test]
    fn runner_output_carries_truncated_flags_and_dropped_counts_per_stream() {
        let change_set = ChangeSet {
            base_revision: "sha256:".into(),
            patch_digest: "sha256:".into(),
            files: Vec::new(),
        };
        let output = RunnerOutput {
            exit_code: Some(0),
            stdout: "stdout text".into(),
            stdout_truncated: true,
            stdout_dropped_bytes: 7,
            stderr: "stderr text".into(),
            stderr_truncated: false,
            stderr_dropped_bytes: 0,
            duration_ms: 1,
            timed_out: false,
            cancelled: false,
            change_set,
        };
        assert!(output.stdout_truncated);
        assert_eq!(output.stdout_dropped_bytes, 7);
        assert!(!output.stderr_truncated);
        assert_eq!(output.stderr_dropped_bytes, 0);
    }
}
