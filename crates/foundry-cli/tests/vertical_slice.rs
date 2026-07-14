use foundry_core::{Graph, JobResult, TaskState};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn plan_attempt_evidence_and_review_form_a_safe_iteration() {
    let root =
        std::env::temp_dir().join(format!("foundry-vertical-slice-{}", uuid::Uuid::new_v4()));
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let podman = bin.join("podman");
    fs::write(
        &podman,
        "#!/bin/sh\nif [ \"$1\" = run ]; then\n  printf 'test suite passed\\n'\n  printf 'artifact' > \"$FAKE_ROOT/output.txt\"\n  exit 0\nfi\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();
    let db = root.join("foundry.sqlite");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let run = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "job-run",
            "--root",
            root.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--task",
            "plans/test.plan.md#1",
            "--idempotency-key",
            "attempt-1",
            "--artifact",
            "output.txt",
            "--",
            "cargo",
            "test",
        ])
        .env("PATH", &path)
        .env("FAKE_ROOT", &root)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let result: JobResult = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(result.spec.as_ref().unwrap().command, ["cargo", "test"]);
    assert_eq!(result.tests.len(), 1);
    assert!(result.tests[0].passed);
    assert_eq!(result.artifacts.len(), 1);
    assert!(
        result
            .change_set
            .as_ref()
            .unwrap()
            .files
            .iter()
            .any(|file| file.path == "output.txt")
    );

    let review = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "review-approve",
            "--db",
            db.to_str().unwrap(),
            "--task",
            "plans/test.plan.md#1",
            "--job",
            &result.job_id.0.to_string(),
            "--reviewer",
            "reviewer@example.test",
            "--reason",
            "captured tests and artifact are sufficient",
        ])
        .output()
        .unwrap();
    assert!(
        review.status.success(),
        "{}",
        String::from_utf8_lossy(&review.stderr)
    );

    let graph = Graph::open(&db).unwrap();
    assert_eq!(
        graph.task_state("plans/test.plan.md#1").unwrap(),
        Some(TaskState::Done)
    );
    assert_eq!(
        graph.job_results_for_task("plans/test.plan.md#1").unwrap(),
        vec![result]
    );
    assert_eq!(
        graph
            .reviews_for_task("plans/test.plan.md#1")
            .unwrap()
            .len(),
        1
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn iterate_waits_for_recorded_review_before_advancing_plan() {
    let root = std::env::temp_dir().join(format!("foundry-iterate-test-{}", uuid::Uuid::new_v4()));
    let bin = root.join("bin");
    let plans = root.join("plans");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&plans).unwrap();
    let podman = bin.join("podman");
    fs::write(
        &podman,
        "#!/bin/sh\n[ \"$1\" = run ] && { echo tests-passed; exit 0; }\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();
    let plan = plans.join("safe.plan.md");
    fs::write(
        &plan,
        "# Safe plan\n\n1. [ ] Run checks - run: cargo test\n",
    )
    .unwrap();
    let db = root.join("foundry.sqlite");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let first = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "iterate",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            root.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        fs::read_to_string(&plan)
            .unwrap()
            .contains("[ ] Run checks")
    );

    let graph = Graph::open(&db).unwrap();
    let task_key = "plans/safe.plan.md#task-2";
    assert_eq!(graph.task_state(task_key).unwrap(), Some(TaskState::Review));
    let result = graph.job_results_for_task(task_key).unwrap().pop().unwrap();
    drop(graph);
    let approved = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "review-approve",
            "--db",
            db.to_str().unwrap(),
            "--task",
            task_key,
            "--job",
            &result.job_id.0.to_string(),
            "--reviewer",
            "reviewer@example.test",
            "--reason",
            "checks passed",
        ])
        .output()
        .unwrap();
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );

    let second = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "iterate",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            root.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        fs::read_to_string(&plan)
            .unwrap()
            .contains("[x] Run checks")
    );
    fs::remove_dir_all(root).unwrap();
}
