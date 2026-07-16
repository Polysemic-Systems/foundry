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

/// Content-complete evidence for every relevant regular file in a workspace.
pub type WorkspaceSnapshot = BTreeMap<String, FileEvidence>;

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
    let after = snapshot_workspace(root)?;
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

/// Workspace `path = "../<sibling>/…"` dependencies live outside the mounted
/// workspace, so a cargo job inside the container cannot see them. Mount each
/// referenced sibling read-only at the container path its relative reference
/// resolves to (`../polysemic` from `/workspace` is `/polysemic`).
fn add_path_dependency_mounts(spec: &JobSpec, root: &Path, args: &mut Vec<String>) -> Result<()> {
    if spec.command.first().map(String::as_str) != Some("cargo") {
        return Ok(());
    }
    let manifest = root.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return Ok(());
    };
    let Some(parent) = root.parent() else {
        return Ok(());
    };
    let mut siblings: Vec<String> = Vec::new();
    for capture in text.split("path = \"../").skip(1) {
        let Some(reference) = capture.split('"').next() else {
            continue;
        };
        let Some(name) = reference.split('/').next() else {
            continue;
        };
        if name.is_empty() || name.contains("..") || siblings.iter().any(|s| s == name) {
            continue;
        }
        siblings.push(name.to_string());
    }
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
                "workspace references path dependency ../{name} but {} does not exist",
                host.display()
            );
        }
        // Attempts reach their siblings through symlinks; mount the target.
        let host = host.canonicalize().unwrap_or(host);
        mounts.extend(["--volume".into(), format!("{}:/{name}:ro", host.display())]);
    }
    args.splice(image_index..image_index, mounts);
    Ok(())
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

fn read_all(mut reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
            "[workspace.dependencies]\n\
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
            "dep = { path = \"../missing/crates/dep\" }\n",
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
}
