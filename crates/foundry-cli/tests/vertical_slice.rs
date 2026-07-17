use foundry_core::{DiscourseAct, DiscourseSpeaker, Graph, JobResult, TaskState};
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
        "#!/bin/sh\nif [ \"$1\" = run ]; then\n  case \" $* \" in\n    *\" rustup toolchain list \"*) printf '1.92.0-test (default)\\n' ;;\n    *) printf 'test suite passed\\n'; printf 'artifact' > \"$FAKE_ROOT/output.txt\" ;;\n  esac\n  exit 0\nfi\nexit 0\n",
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
            "--json",
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
            "--root",
            root.to_str().unwrap(),
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
    let discourse = graph
        .discourse_for_context(&format!("review:{}", result.job_id.0))
        .unwrap();
    assert_eq!(discourse.len(), 2);
    assert_eq!(discourse[0].act, DiscourseAct::Question);
    assert_eq!(discourse[1].speaker, DiscourseSpeaker::Human);
    assert_eq!(discourse[1].act, DiscourseAct::Synthesis);
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
        "#!/bin/sh\nif [ \"$1\" = run ]; then\n  case \" $* \" in\n    *\" rustup toolchain list \"*) echo '1.92.0-test (default)' ;;\n    *) echo tests-passed ;;\n  esac\n  exit 0\nfi\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();
    let plan = plans.join("safe.plan.md");
    fs::write(
        &plan,
        "# Safe plan\n\n1. [ ] Run checks - run: cargo test - id: run-checks\n",
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
    let first_output = String::from_utf8_lossy(&first.stdout);
    assert!(first_output.contains("Foundry verification"));
    assert!(first_output.contains("Status    PASSED"));
    assert!(!first_output.contains("\"stdout\":"));
    assert!(
        first_output.contains("never been observed failing"),
        "plain iterate must warn that a pass may be vacuous: {first_output}"
    );
    assert!(
        fs::read_to_string(&plan)
            .unwrap()
            .contains("[ ] Run checks")
    );

    let graph = Graph::open(&db).unwrap();
    let task_key = "plans/safe.plan.md#run-checks";
    assert_eq!(graph.task_state(task_key).unwrap(), Some(TaskState::Review));
    let result = graph.job_results_for_task(task_key).unwrap().pop().unwrap();
    assert_eq!(
        result.acceptance_authority.as_deref(),
        Some("unfalsified"),
        "plain iterate evidence must record that the check never failed"
    );
    drop(graph);
    let approved = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "review-approve",
            "--root",
            root.to_str().unwrap(),
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

/// The TDD tests exercise real Bubblewrap isolation. Environments without
/// bwrap (e.g. Foundry verifying itself inside its own Podman runner) skip
/// them with a named reason; CI installs bwrap and enforces full coverage.
#[test]
fn require_falsified_refuses_a_check_that_was_never_observed_failing() {
    let root = std::env::temp_dir().join(format!(
        "foundry-require-falsified-test-{}",
        uuid::Uuid::new_v4()
    ));
    let plans = root.join("plans");
    fs::create_dir_all(&plans).unwrap();
    let plan = plans.join("strict.plan.md");
    fs::write(
        &plan,
        "# Strict plan\n\n1. [ ] Run checks - run: cargo test - id: run-checks\n",
    )
    .unwrap();
    let db = root.join("foundry.sqlite");

    let refused = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "iterate",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            root.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--require-falsified",
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "plain iterate with --require-falsified must refuse"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("never observed failing"),
        "the refusal must explain the falsifiability rule: {stderr}"
    );

    // Nothing was staged or recorded: the refusal happened before the run.
    let graph = Graph::open(&db).unwrap();
    assert_eq!(
        graph
            .job_results_for_task("plans/strict.plan.md#run-checks")
            .unwrap(),
        vec![]
    );
    fs::remove_dir_all(root).unwrap();
}

fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .is_ok()
}

#[test]
fn tdd_job_evidence_captures_changes_made_before_sandbox_verification() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap unavailable in this environment");
        return;
    }
    let root = std::env::temp_dir().join(format!(
        "foundry-agent-evidence-test-{}",
        uuid::Uuid::new_v4()
    ));
    let bin = root.join("bin");
    let plans = root.join("plans");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&plans).unwrap();

    let podman = bin.join("podman");
    fs::write(
        &podman,
        "#!/bin/sh\n\
         if [ \"$1\" = run ]; then\n\
           case \" $* \" in\n\
             *\" rustup toolchain list \"*) echo '1.92.0-test (default)'; exit 0 ;;\n\
           esac\n\
           count_file=\"$FAKE_ROOT/podman-count\"\n\
           count=0\n\
           [ -f \"$count_file\" ] && count=$(cat \"$count_file\")\n\
           count=$((count + 1))\n\
           echo \"$count\" > \"$count_file\"\n\
           if [ \"$count\" -eq 2 ]; then echo 'expected red failure' >&2; exit 1; fi\n\
           echo 'test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s'\n\
           exit 0\n\
         fi\n\
         exit 0\n",
    )
    .unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();

    let agent = bin.join("fake-agent");
    fs::write(
        &agent,
        "#!/bin/sh\n\
         prompt=$(cat)\n\
         case \"$prompt\" in\n\
           *\"test-writing phase\"*) mkdir -p tests; echo test > tests/agent_test.rs ;;\n\
           *) echo 'agent-authored change' > agent-change.txt ;;\n\
         esac\n",
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();

    let plan = plans.join("capture.plan.md");
    fs::write(
        &plan,
        "# Capture plan\n\n1. [ ] Capture editor changes - run: cargo test - id: capture-editor-changes\n",
    )
    .unwrap();
    let db = root.join("foundry.sqlite");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let run = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .args([
            "iterate",
            "--tdd",
            "--agent-command",
            agent.to_str().unwrap(),
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            root.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ])
        .env("PATH", &path)
        .env("FAKE_ROOT", &root)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );

    let graph = Graph::open(&db).unwrap();
    let result = graph
        .job_results_for_task("plans/capture.plan.md#capture-editor-changes")
        .unwrap()
        .pop()
        .expect("verification job evidence");
    let changes = result.change_set.expect("job change set");
    assert!(
        changes.files.iter().any(|file| {
            file.path == "agent-change.txt" && file.status == foundry_core::ChangeStatus::Added
        }),
        "agent edit must be captured from the pre-agent baseline: {:?}",
        changes.files
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tdd_job_evidence_keeps_the_pre_agent_baseline_across_failed_verification_retries() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap unavailable in this environment");
        return;
    }
    let root = std::env::temp_dir().join(format!(
        "foundry-agent-retry-evidence-test-{}",
        uuid::Uuid::new_v4()
    ));
    let bin = root.join("bin");
    let plans = root.join("plans");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&plans).unwrap();

    let podman = bin.join("podman");
    fs::write(
        &podman,
        "#!/bin/sh\n\
         if [ \"$1\" = run ]; then\n\
           case \" $* \" in\n\
             *\" rustup toolchain list \"*) echo '1.92.0-test (default)'; exit 0 ;;\n\
           esac\n\
           count_file=\"$FAKE_ROOT/podman-count\"\n\
           count=0\n\
           [ -f \"$count_file\" ] && count=$(cat \"$count_file\")\n\
           count=$((count + 1))\n\
           echo \"$count\" > \"$count_file\"\n\
           case \"$count\" in\n\
             2) echo 'expected red failure' >&2; exit 1 ;;\n\
             3) echo 'error: verification failed' >&2; exit 101 ;;\n\
             *) echo 'test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s'; exit 0 ;;\n\
           esac\n\
         fi\n\
         exit 0\n",
    )
    .unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();

    let agent = bin.join("fake-agent");
    fs::write(
        &agent,
        "#!/bin/sh\n\
         prompt=$(cat)\n\
         case \"$prompt\" in\n\
           *\"test-writing phase\"*) mkdir -p tests; echo test > tests/agent_test.rs ;;\n\
           *\"implementation phase\"*) echo initial > initial-agent-change.txt ;;\n\
           *) echo repair > repair-agent-change.txt ;;\n\
         esac\n",
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();

    let plan = plans.join("retry-capture.plan.md");
    fs::write(
        &plan,
        "# Retry capture plan\n\n1. [ ] Capture edits across retries - run: cargo test - id: capture-edits-across-retries\n",
    )
    .unwrap();
    let db = root.join("foundry.sqlite");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
            .args([
                "iterate",
                "--tdd",
                "--agent-command",
                agent.to_str().unwrap(),
                "--plan",
                plan.to_str().unwrap(),
                "--root",
                root.to_str().unwrap(),
                "--db",
                db.to_str().unwrap(),
            ])
            .env("PATH", &path)
            .env("FAKE_ROOT", &root)
            .output()
            .unwrap()
    };

    let failed = run();
    assert!(!failed.status.success(), "first verification must fail");
    assert!(root.join(".foundry/tdd-baselines").is_dir());

    let repaired = run();
    assert!(
        repaired.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&repaired.stdout),
        String::from_utf8_lossy(&repaired.stderr)
    );

    let graph = Graph::open(&db).unwrap();
    let result = graph
        .job_results_for_task("plans/retry-capture.plan.md#capture-edits-across-retries")
        .unwrap()
        .into_iter()
        .find(|result| result.state == foundry_core::JobState::Succeeded)
        .expect("successful retry evidence");
    let changes = result.change_set.expect("retry change set");
    for expected in ["initial-agent-change.txt", "repair-agent-change.txt"] {
        assert!(
            changes.files.iter().any(|file| file.path == expected),
            "{expected} must survive the process boundary in evidence: {:?}",
            changes.files
        );
    }
    assert_eq!(
        fs::read_dir(root.join(".foundry/tdd-baselines"))
            .unwrap()
            .count(),
        0,
        "a successful verification clears its persisted baseline"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn staged_tdd_changes_reach_the_authoritative_workspace_only_after_approval() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap unavailable in this environment");
        return;
    }
    let root = std::env::temp_dir().join(format!(
        "foundry-staged-promotion-test-{}",
        uuid::Uuid::new_v4()
    ));
    let bin = root.join("bin");
    let plans = root.join("plans");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&plans).unwrap();
    fs::write(root.join("source.txt"), "authoritative\n").unwrap();

    let podman = bin.join("podman");
    fs::write(
        &podman,
        "#!/bin/sh\n\
         if [ \"$1\" = run ]; then\n\
           case \" $* \" in\n\
             *\" rustup toolchain list \"*) echo '1.92.0-test (default)'; exit 0 ;;\n\
           esac\n\
           count_file=\"$FAKE_ROOT/podman-count\"\n\
           count=0\n\
           [ -f \"$count_file\" ] && count=$(cat \"$count_file\")\n\
           count=$((count + 1))\n\
           echo \"$count\" > \"$count_file\"\n\
           if [ $((count % 3)) -eq 2 ]; then echo 'expected red failure' >&2; exit 1; fi\n\
           echo 'test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s'\n\
           exit 0\n\
         fi\n\
         exit 0\n",
    )
    .unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();

    let agent = bin.join("fake-agent");
    fs::write(
        &agent,
        "#!/bin/sh\n\
         prompt=$(cat)\n\
         case \"$prompt\" in\n\
           *\"test-writing phase\"*) mkdir -p tests; echo test > tests/agent_test.rs ;;\n\
           *\"first approach is not acceptable\"*) echo \"staged attempt 2\" > source.txt ;;\n\
           *) echo \"staged attempt 1\" > source.txt ;;\n\
         esac\n",
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();

    let plan = plans.join("promotion.plan.md");
    fs::write(
        &plan,
        "# Promotion plan\n\n1. [ ] Stage safely - run: cargo test - id: stage-safely\n",
    )
    .unwrap();
    let db = root.join("foundry.sqlite");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let iterate = || {
        Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
            .args([
                "iterate",
                "--tdd",
                "--agent-command",
                agent.to_str().unwrap(),
                "--plan",
                plan.to_str().unwrap(),
                "--root",
                root.to_str().unwrap(),
                "--db",
                db.to_str().unwrap(),
            ])
            .env("PATH", &path)
            .env("FAKE_ROOT", &root)
            .output()
            .unwrap()
    };
    let review = |command: &str, job: uuid::Uuid, reason: &str| {
        Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
            .args([
                command,
                "--root",
                root.to_str().unwrap(),
                "--db",
                db.to_str().unwrap(),
                "--task",
                "plans/promotion.plan.md#stage-safely",
                "--job",
                &job.to_string(),
                "--reviewer",
                "human@example.test",
                "--reason",
                reason,
            ])
            .output()
            .unwrap()
    };

    let first = iterate();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let graph = Graph::open(&db).unwrap();
    let first_job = graph
        .job_results_for_task("plans/promotion.plan.md#stage-safely")
        .unwrap()
        .pop()
        .unwrap();
    assert!(first_job.staged);
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "authoritative\n",
        "successful verification is still only advisory evidence"
    );

    let rejected = review(
        "review-reject",
        first_job.job_id.0,
        "the first approach is not acceptable",
    );
    assert!(rejected.status.success());
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "authoritative\n",
        "rejection must not promote staged edits"
    );

    let second = iterate();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let graph = Graph::open(&db).unwrap();
    let second_job = graph
        .job_results_for_task("plans/promotion.plan.md#stage-safely")
        .unwrap()
        .into_iter()
        .last()
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "authoritative\n"
    );

    let approved = review(
        "review-approve",
        second_job.job_id.0,
        "the second evidence bundle is sufficient",
    );
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "staged attempt 2\n"
    );
    assert_eq!(
        Graph::open(&db)
            .unwrap()
            .task_state("plans/promotion.plan.md#stage-safely")
            .unwrap(),
        Some(TaskState::Done)
    );
    fs::remove_dir_all(root).unwrap();
}
