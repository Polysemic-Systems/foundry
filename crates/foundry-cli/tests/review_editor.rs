//! The review TUI must hand editing off to the reviewer's normal editor.
//!
//! This test uses a pseudo-terminal because leaving raw mode and the alternate
//! screen is part of the observable contract: an editor launched while the TUI
//! still owns either would be unusable even if the draft file round-tripped.

use foundry_core::{Graph, JobResult, JobState, TaskState};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const TASK: &str = "review-editor-test";
const ORIGINAL_DRAFT: &str = "\
Shared question
Does the evidence justify approval?

Observed evidence
- the original draft is passed to the editor
";
const EDITED_DRAFT: &str = "\
Shared question
Should the compatibility concern block approval?

Observed evidence
- the editor can replace text anywhere in the draft

Synthesis
Reject pending compatibility evidence.
";
const EDITOR_SENTINEL: &[u8] = b"EDITOR_IS_VISIBLE";
const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("foundry-review-editor-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn bin(&self) -> PathBuf {
        self.0.join("bin")
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn setup_environment(root: &ScratchDir) -> (PathBuf, PathBuf, PathBuf) {
    fs::create_dir_all(root.bin()).unwrap();

    let podman = write_script(
        &root.bin(),
        "podman",
        "#!/bin/sh\n\
         if [ \"$1\" = run ]; then\n\
             printf 'test suite passed\\n'\n\
             printf 'artifact' > \"$FAKE_ROOT/output.txt\"\n\
             exit 0\n\
         fi\n\
         if [ \"$1\" = image ]; then\n\
             printf 'sha256:faketestimageid\\n'\n\
             exit 0\n\
         fi\n\
         exit 0\n",
    );

    let editor = write_script(
        &root.bin(),
        "fake-editor",
        &format!(
            "#!/bin/sh\n\
             file=\"$1\"\n\
             if [ -s \"$file\" ]; then\n\
                 echo 'editor: rationale was pre-populated from advisory prose' >&2\n\
                 exit 1\n\
             fi\n\
             cat > \"$file\" <<'EOF'\n{}EOF\n\
             printf '{}'\n",
            EDITED_DRAFT,
            String::from_utf8_lossy(EDITOR_SENTINEL)
        ),
    );

    let agent = write_script(
        &root.bin(),
        "fake-agent",
        r#"#!/bin/sh
printf '%s' '{"recommendation": "reject", "body": "Shared question\nCan the change be falsified?\n\nObserved evidence\n- none\n\nAssumptions\n- the model is adversarial\n\nCompeting interpretation\n- the change is unsafe\n\nFalsifying evidence\n- a counterexample\n\nQuestion for the human\n- what would convince you?\n\nSynthesis\nReject unless falsified."}'
"#,
    );

    (podman, editor, agent)
}

fn run_job(root: &ScratchDir, db: &Path) -> JobResult {
    let path = format!(
        "{}:{}",
        root.bin().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "job-run",
            "--root",
            root.0.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--task",
            TASK,
            "--idempotency-key",
            "review-editor-attempt",
            "--artifact",
            "output.txt",
            "--json",
            "--",
            "cargo",
            "test",
        ])
        .env("PATH", &path)
        .env("FAKE_ROOT", &root.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn seed_prior_review(root: &ScratchDir, db: &Path, job_id: foundry_core::JobId) {
    let path = format!(
        "{}:{}",
        root.bin().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "review-reject",
            "--root",
            root.0.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--task",
            TASK,
            "--job",
            &job_id.0.to_string(),
            "--reviewer",
            "test@example.test",
            "--reason",
            ORIGINAL_DRAFT,
        ])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn spawn_reader(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = tx.send(buf[..n].to_vec());
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// Wait until `needle` appears in the pty stream. Bytes are accumulated in
/// `carry`, which persists across calls: a single pty read may contain both
/// the sequence one call waits for and the one the next call waits for (the
/// editor sentinel is written sub-milliseconds around the alternate-screen
/// switches), so unconsumed bytes must survive for the next call to scan.
fn wait_for(
    rx: &mpsc::Receiver<Vec<u8>>,
    carry: &mut Vec<u8>,
    needle: &[u8],
    timeout: Duration,
    context: &str,
) -> Vec<u8> {
    let start = Instant::now();
    loop {
        if let Some(position) = carry
            .windows(needle.len())
            .position(|window| window == needle)
        {
            return carry.drain(..position + needle.len()).collect();
        }
        if start.elapsed() >= timeout {
            panic!(
                "timed out after {:?} waiting for {} in pty output; accumulated: {:?}",
                timeout,
                context,
                String::from_utf8_lossy(carry)
            );
        }
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) {
            carry.extend_from_slice(&chunk);
        }
    }
}

#[test]
fn review_edit_suspends_the_tui_and_replaces_the_draft_via_editor() {
    let root = ScratchDir::new();
    let db = root.0.join("foundry.sqlite");
    let (_podman, editor, agent) = setup_environment(&root);

    let result = run_job(&root, &db);
    assert_eq!(result.state, JobState::Succeeded);
    seed_prior_review(&root, &db, result.job_id);

    // Sanity: the prior review is present and the task left Review state.
    {
        let graph = Graph::open(&db).unwrap();
        assert_ne!(graph.task_state(TASK).unwrap(), Some(TaskState::Review));
        let reviews = graph.reviews_for_task(TASK).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].reason, ORIGINAL_DRAFT);
    }

    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let path = format!(
        "{}:{}",
        root.bin().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_foundry-cli"));
    cmd.arg("review-tui");
    cmd.arg("--root");
    cmd.arg(root.0.to_str().unwrap());
    cmd.arg("--db");
    cmd.arg(db.to_str().unwrap());
    cmd.arg("--task");
    cmd.arg(TASK);
    cmd.arg("--job");
    cmd.arg(result.job_id.0.to_string());
    cmd.arg("--reviewer");
    cmd.arg("test@example.test");
    cmd.env("EDITOR", editor.to_str().unwrap());
    cmd.env("PATH", &path);
    cmd.env("FOUNDRY_REVIEW_AGENT_COMMAND", agent.to_str().unwrap());
    cmd.env("FOUNDRY_AGENT_SANDBOX", "off");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn review-tui");
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().expect("clone pty reader");
    let rx = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take pty writer");
    let mut pty_output = Vec::new();

    // Wait for the TUI to enter the alternate screen.
    wait_for(
        &rx,
        &mut pty_output,
        ENTER_ALTERNATE_SCREEN,
        Duration::from_secs(5),
        "TUI enter alternate screen",
    );

    // Choosing a partner answers only the advisory-choice question. It must
    // not copy that partner's prose or recommendation into the human answer.
    writer.write_all(b"1").unwrap();
    writer.flush().unwrap();

    // Selection alone must not sign the review. If it did, the process would
    // exit here and the editor sentinel below could never be observed.
    writer.write_all(b"s").unwrap();
    writer.flush().unwrap();

    // Open the external editor.
    writer.write_all(b"e").unwrap();
    writer.flush().unwrap();

    // The TUI must leave the alternate screen before the editor can be used.
    wait_for(
        &rx,
        &mut pty_output,
        LEAVE_ALTERNATE_SCREEN,
        Duration::from_secs(5),
        "TUI leave alternate screen",
    );

    // Wait for the editor to become visible, which proves it actually ran.
    wait_for(
        &rx,
        &mut pty_output,
        EDITOR_SENTINEL,
        Duration::from_secs(5),
        "editor sentinel",
    );

    // After the editor exits, the TUI must resume the alternate screen.
    wait_for(
        &rx,
        &mut pty_output,
        ENTER_ALTERNATE_SCREEN,
        Duration::from_secs(5),
        "TUI resume alternate screen",
    );

    // Save the edited synthesis.
    writer.write_all(b"s").unwrap();
    writer.flush().unwrap();

    let status = child.wait().expect("wait for review-tui");
    assert!(status.success(), "review-tui exited with {status:?}");

    // The retrospective resolution must carry the editor's output, not the
    // original draft the TUI started with.
    let graph = Graph::open(&db).unwrap();
    let lessons = graph.review_lessons_for_task(TASK, 1).unwrap();
    let latest = lessons
        .first()
        .expect("a retrospective resolution should have been recorded");
    assert!(
        latest.contains(EDITED_DRAFT),
        "latest lesson did not contain edited draft: {latest}"
    );
}
