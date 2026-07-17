use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, db: &Path, command: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .arg(command)
        .args(["--root", root.to_str().unwrap()])
        .args(["--db", db.to_str().unwrap()])
        .output()
        .unwrap()
}

#[test]
fn index_rebuild_and_heal_refuse_without_the_repository_lease() {
    let root = std::env::temp_dir().join(format!("foundry-index-lease-{}", uuid::Uuid::new_v4()));
    let foundry = root.join(".foundry");
    let db = foundry.join("db.sqlite");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(&foundry).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn indexed() {}\n").unwrap();

    let seeded = run(&root, &db, "index");
    assert!(
        seeded.status.success(),
        "{}",
        String::from_utf8_lossy(&seeded.stderr)
    );

    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(foundry.join("repository.lease"))
        .unwrap();
    lease.lock().unwrap();

    for command in ["index", "rebuild", "heal"] {
        let refused = run(&root, &db, command);
        assert!(
            !refused.status.success(),
            "{command} unexpectedly mutated under a held lease"
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("holds the repository lease"),
            "{command} refusal was not lease-specific: {}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }

    lease.unlock().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn direct_job_run_refuses_before_starting_a_container_when_lease_is_held() {
    let root = std::env::temp_dir().join(format!("foundry-job-lease-{}", uuid::Uuid::new_v4()));
    let foundry = root.join(".foundry");
    let db = foundry.join("db.sqlite");
    fs::create_dir_all(&foundry).unwrap();
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(foundry.join("repository.lease"))
        .unwrap();
    lease.lock().unwrap();

    let refused = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .arg("job-run")
        .args(["--root", root.to_str().unwrap()])
        .args(["--db", db.to_str().unwrap()])
        .args(["--task", "plans/test.plan.md#leased"])
        .args(["--", "cargo", "test"])
        .output()
        .unwrap();

    assert!(
        !refused.status.success(),
        "job-run unexpectedly reached the runner under a held lease"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("holds the repository lease"),
        "job-run refusal was not lease-specific: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !db.exists(),
        "lease refusal must happen before graph lifecycle mutation"
    );

    lease.unlock().unwrap();
    fs::remove_dir_all(root).unwrap();
}
