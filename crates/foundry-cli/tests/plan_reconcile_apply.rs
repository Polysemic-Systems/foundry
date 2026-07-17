use foundry_core::{Event, Graph, TaskState};
use std::fs::{self, OpenOptions};
use std::process::{Command, Output};

fn run_reconcile(root: &std::path::Path, apply: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_foundry-cli"));
    command.args([
        "reconcile-plan",
        "--root",
        root.to_str().unwrap(),
        "--plan",
        root.join("plans/features.plan.md").to_str().unwrap(),
        "--db",
        root.join(".foundry/db.sqlite").to_str().unwrap(),
    ]);
    if apply {
        command.arg("--apply");
    }
    command.output().unwrap()
}

#[test]
fn reconcile_plan_apply_refuses_under_lease_then_repairs_once() {
    let root = std::env::temp_dir().join(format!(
        "foundry-reconcile-apply-test-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(root.join("plans")).unwrap();
    fs::create_dir_all(root.join(".foundry")).unwrap();
    let plan_path = root.join("plans/features.plan.md");
    let original = "\
# Features

1. [ ] Repair durable state - id: durable-state
";
    fs::write(&plan_path, original).unwrap();
    let db = root.join(".foundry/db.sqlite");
    let mut graph = Graph::open(&db).unwrap();
    let old_key = "plans/features.plan.md#task-2";
    let new_key = "plans/features.plan.md#durable-state";
    graph
        .initialize_task_state(old_key, TaskState::Done)
        .unwrap();
    drop(graph);

    let lease_path = root.join(".foundry/repository.lease");
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lease_path)
        .unwrap();
    lease.lock().unwrap();

    let refused = run_reconcile(&root, true);
    assert!(
        !refused.status.success(),
        "apply must refuse while another process holds the repository lease"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("repository lease"),
        "refusal must identify the gate: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&refused.stdout).contains("LEGACY KEY"),
        "apply must acquire the lease before reading and reporting a repair plan"
    );
    assert_eq!(fs::read_to_string(&plan_path).unwrap(), original);
    let graph = Graph::open(&db).unwrap();
    assert_eq!(graph.task_state(old_key).unwrap(), Some(TaskState::Done));
    assert_eq!(graph.task_state(new_key).unwrap(), None);
    drop(graph);
    std::fs::File::unlock(&lease).unwrap();

    let applied = run_reconcile(&root, true);
    assert!(
        applied.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    let repaired_plan = fs::read_to_string(&plan_path).unwrap();
    assert!(
        repaired_plan.contains("[x] Repair durable state - id: durable-state"),
        "one apply must synchronize completion after migrating the key: {repaired_plan}"
    );
    let graph = Graph::open(&db).unwrap();
    assert_eq!(graph.task_state(old_key).unwrap(), None);
    assert_eq!(graph.task_state(new_key).unwrap(), Some(TaskState::Done));
    let reconciled_events = graph
        .events(100)
        .unwrap()
        .into_iter()
        .filter(|(_, event)| matches!(event, Event::PlanReconciled { .. }))
        .count();
    assert_eq!(reconciled_events, 1);
    drop(graph);

    let reapplied = run_reconcile(&root, true);
    assert!(
        reapplied.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&reapplied.stdout),
        String::from_utf8_lossy(&reapplied.stderr)
    );
    assert!(
        String::from_utf8_lossy(&reapplied.stdout).contains("Plan and graph agree"),
        "idempotent re-apply must report a clean graph"
    );
    assert_eq!(fs::read_to_string(&plan_path).unwrap(), repaired_plan);
    let graph = Graph::open(&db).unwrap();
    let reconciled_events = graph
        .events(100)
        .unwrap()
        .into_iter()
        .filter(|(_, event)| matches!(event, Event::PlanReconciled { .. }))
        .count();
    assert_eq!(
        reconciled_events, 1,
        "a clean re-apply must not append a second mutation event"
    );

    fs::remove_dir_all(root).unwrap();
}
