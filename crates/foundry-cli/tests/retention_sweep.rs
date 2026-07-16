//! End-to-end retention enforcement: governed evidence with an expired
//! DeleteAfter policy is erased through the lethe erasure contract; retained
//! evidence, append-only history, and mid-review tasks are untouched.

use foundry_core::{Event, Graph, JobId, JobResult, RetentionPolicy};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn fake_podman(bin: &std::path::Path) {
    let podman = bin.join("podman");
    fs::write(
        &podman,
        "#!/bin/sh\nif [ \"$1\" = run ]; then\n  case \" $* \" in\n    *\" rustup toolchain list \"*) printf '1.92.0-test (default)\\n' ;;\n    *) printf 'test suite passed\\n'; printf '%s' \"$FAKE_CONTENT\" > \"$FAKE_ROOT/$FAKE_FILE\" ;;\n  esac\n  exit 0\nfi\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Cli<'a> {
    root: &'a std::path::Path,
    db: &'a std::path::Path,
    path: String,
}

impl Cli<'_> {
    fn job_run(
        &self,
        task: &str,
        file: &str,
        content: &str,
        retention_days: Option<&str>,
    ) -> JobResult {
        let mut args = vec![
            "job-run".to_string(),
            "--root".into(),
            self.root.to_str().unwrap().into(),
            "--db".into(),
            self.db.to_str().unwrap().into(),
            "--task".into(),
            task.into(),
            "--idempotency-key".into(),
            format!("attempt-{file}"),
            "--artifact".into(),
            file.into(),
            "--json".into(),
        ];
        if let Some(days) = retention_days {
            args.push("--evidence-retention-days".into());
            args.push(days.into());
        }
        args.extend(["--".into(), "cargo".into(), "test".into()]);
        let run = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
            .args(&args)
            .env("PATH", &self.path)
            .env("FAKE_ROOT", self.root)
            .env("FAKE_FILE", file)
            .env("FAKE_CONTENT", content)
            .env_remove("FOUNDRY_EVIDENCE_RETENTION_DAYS")
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        serde_json::from_slice(&run.stdout).unwrap()
    }

    fn approve(&self, task: &str, job: JobId) {
        let review = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
            .args([
                "review-approve",
                "--root",
                self.root.to_str().unwrap(),
                "--db",
                self.db.to_str().unwrap(),
                "--task",
                task,
                "--job",
                &job.0.to_string(),
                "--reviewer",
                "reviewer@example.test",
                "--reason",
                "evidence sufficient",
            ])
            .output()
            .unwrap();
        assert!(
            review.status.success(),
            "{}",
            String::from_utf8_lossy(&review.stderr)
        );
    }

    fn sweep(&self, enforce: bool) -> serde_json::Value {
        let mut args = vec![
            "sweep".to_string(),
            "--db".into(),
            self.db.to_str().unwrap().into(),
            "--json".into(),
        ];
        if enforce {
            args.push("--enforce".into());
        }
        let sweep = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
            .args(&args)
            .output()
            .unwrap();
        assert!(
            sweep.status.success(),
            "{}",
            String::from_utf8_lossy(&sweep.stderr)
        );
        serde_json::from_slice(&sweep.stdout).unwrap()
    }
}

#[test]
fn sweep_erases_only_due_evidence_and_leaves_receipts() {
    let root = std::env::temp_dir().join(format!("foundry-retention-{}", uuid::Uuid::new_v4()));
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fake_podman(&bin);
    let db_dir = root.join(".foundry");
    fs::create_dir_all(&db_dir).unwrap();
    let db = db_dir.join("db.sqlite");
    let cli = Cli {
        root: &root,
        db: &db,
        path: format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    };

    // Job A: delete-due evidence on an approved task — the sweep's target.
    let result_a = cli.job_run(
        "plans/test.plan.md#1",
        "a.txt",
        "exclusive to job a",
        Some("0"),
    );
    assert!(
        matches!(
            result_a.governance.retention,
            RetentionPolicy::DeleteAfter { .. }
        ),
        "retention flag must opt evidence into DeleteAfter"
    );
    cli.approve("plans/test.plan.md#1", result_a.job_id);
    // Job B: default 30-day review policy — must survive and still hydrate.
    let result_b = cli.job_run("plans/test.plan.md#2", "b.txt", "belongs to job b", None);
    cli.approve("plans/test.plan.md#2", result_b.job_id);
    // Job C: delete-due, but its task stays in review — must be deferred.
    let result_c = cli.job_run(
        "plans/test.plan.md#3",
        "c.txt",
        "belongs to job c",
        Some("0"),
    );

    // Dry run reports without deleting.
    let report = cli.sweep(false);
    assert_eq!(report["delete_due"].as_array().unwrap().len(), 1);
    assert_eq!(report["deferred"].as_array().unwrap().len(), 1);
    {
        let graph = Graph::open(&db).unwrap();
        assert!(graph.job_result_exists(result_a.job_id).unwrap());
    }

    // Blobs younger than the sweep's age guard are presumed owned by a job
    // that is still externalizing and are left alone; age this test's blobs
    // past the guard so enforcement treats them as settled evidence.
    {
        let graph = Graph::open(&db).unwrap();
        let blob_root = graph.blob_store_root().unwrap().unwrap();
        for entry in fs::read_dir(&blob_root).unwrap() {
            let path = entry.unwrap().path();
            let aged = Command::new("touch")
                .args(["-d", "2 hours ago"])
                .arg(&path)
                .status()
                .unwrap();
            assert!(aged.success());
        }
    }

    // Enforcement erases job A only.
    let enforced = cli.sweep(true);
    assert_eq!(enforced["deleted"].as_u64(), Some(1));
    assert_eq!(enforced["deferred"].as_u64(), Some(1));
    assert_eq!(enforced["receipts"].as_array().unwrap().len(), 1);
    let receipt = &enforced["receipts"][0];
    assert_eq!(receipt["status"].as_str(), Some("complete"));
    assert!(
        receipt["receipt"]
            .as_str()
            .unwrap()
            .starts_with("lethe://request/")
    );

    let graph = Graph::open(&db).unwrap();
    assert!(!graph.job_result_exists(result_a.job_id).unwrap());
    assert!(graph.job_result_exists(result_c.job_id).unwrap());
    let survivors = graph.job_results_for_task("plans/test.plan.md#2").unwrap();
    assert_eq!(survivors, vec![result_b], "job B must hydrate unchanged");

    // Job A's blob is gone from the store; job B's and C's remain.
    let blob_root = graph.blob_store_root().unwrap().unwrap();
    let blob_path = |content: &[u8]| {
        use sha2::Digest;
        blob_root.join(format!("{:x}", sha2::Sha256::digest(content)))
    };
    assert!(!blob_path(b"exclusive to job a").exists());
    assert!(blob_path(b"belongs to job b").exists());
    assert!(blob_path(b"belongs to job c").exists());

    // History is append-only: the erased job's task decision is retained and
    // the receipts landed in the event log.
    assert_eq!(
        graph
            .reviews_for_task("plans/test.plan.md#1")
            .unwrap()
            .len(),
        1
    );
    let kinds: Vec<&'static str> = graph
        .events(100)
        .unwrap()
        .into_iter()
        .map(|(_, event)| event.kind())
        .collect();
    assert!(kinds.contains(&"evidence_erased"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"retention_swept"), "kinds: {kinds:?}");
    let erased_events: Vec<Event> = graph
        .events(100)
        .unwrap()
        .into_iter()
        .map(|(_, event)| event)
        .filter(|event| event.kind() == "evidence_erased")
        .collect();
    assert_eq!(erased_events.len(), 1);
    if let Event::EvidenceErased {
        job_id, receipt, ..
    } = &erased_events[0]
    {
        assert_eq!(*job_id, result_a.job_id);
        assert!(receipt.starts_with("lethe://request/"));
    }
    drop(graph);

    // A replayed enforcement converges: nothing left to erase, exit zero,
    // and a second sweep event is appended — history never mutates.
    let replay = cli.sweep(true);
    assert_eq!(replay["deleted"].as_u64(), Some(0));
    assert_eq!(replay["delete_due"].as_u64(), Some(0));
    let graph = Graph::open(&db).unwrap();
    let sweep_events = graph
        .events(100)
        .unwrap()
        .into_iter()
        .filter(|(_, event)| event.kind() == "retention_swept")
        .count();
    assert_eq!(sweep_events, 2);
    drop(graph);

    fs::remove_dir_all(root).unwrap();
}
