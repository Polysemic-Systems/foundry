use foundry_core::{Graph, NodeKind};
use std::fs;
use std::process::{Command, Output};

fn iterate(root: &std::path::Path, plan: Option<&std::path::Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_foundry-cli"));
    command.current_dir(root).args([
        "iterate",
        "--root",
        root.to_str().unwrap(),
        "--db",
        root.join(".foundry/db.sqlite").to_str().unwrap(),
    ]);
    if let Some(plan) = plan {
        command.args(["--plan", plan.to_str().unwrap()]);
    }
    command.output().unwrap()
}

#[test]
fn bare_iterate_selects_features_plan_and_derives_its_title_from_path() {
    let root = std::env::temp_dir().join(format!("foundry-plan-default-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("plans")).unwrap();
    let bootstrap = "# Bootstrap\n\n1. [x] Historical task\n";
    fs::write(root.join("plans/bootstrap.plan.md"), bootstrap).unwrap();
    fs::write(
        root.join("plans/features.plan.md"),
        "# Features\n\n1. [ ] Review feature - stop: human - id: review-feature\n",
    )
    .unwrap();

    let output = iterate(&root, None);
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Next task: Review feature"),
        "bare iterate must operate on the active feature backlog"
    );
    assert_eq!(
        fs::read_to_string(root.join("plans/bootstrap.plan.md")).unwrap(),
        bootstrap,
        "bare iterate must not rewrite the completed historical bootstrap plan"
    );
    let graph = Graph::open(&root.join(".foundry/db.sqlite")).unwrap();
    let plan = graph
        .find_node_by_name(NodeKind::Plan, "plans/features.plan.md")
        .unwrap()
        .unwrap();
    assert_eq!(plan.payload["title"], "features");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn completed_plan_is_not_rewritten_only_to_persist_derived_ids() {
    let root = std::env::temp_dir().join(format!("foundry-plan-history-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("plans")).unwrap();
    let historical = root.join("plans/historical.plan.md");
    let original = "# Historical\n\n1. [x] Completed long ago\n";
    fs::write(&historical, original).unwrap();

    let output = iterate(&root, Some(&historical));
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&historical).unwrap(),
        original,
        "completed history has no executable consumer for newly persisted ids"
    );

    fs::remove_dir_all(root).unwrap();
}
