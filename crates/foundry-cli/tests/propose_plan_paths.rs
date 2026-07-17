#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn write_fake_curl(path: &Path) {
    fs::write(
        path,
        r#"#!/bin/sh
payload=$(cat)
case "$payload" in
  *proposal-mode*)
    printf '%s\n' '{"message":{"content":"{\"spec\":\"Create a repository-local plan.\",\"tasks\":[{\"description\":\"Record the approved feature\"}]}"},"prompt_eval_count":1,"eval_count":1}'
    ;;
  *)
    printf '%s\n' '{"message":{"content":"Which repository should own the new plan?"},"prompt_eval_count":1,"eval_count":1}'
    ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn propose_new_plan_with_a_relative_path_writes_beneath_root_without_changing_process_cwd() {
    let scratch = std::env::temp_dir().join(format!(
        "foundry-propose-plan-path-{}",
        uuid::Uuid::new_v4()
    ));
    let root = scratch.join("workspace");
    let invocation_dir = scratch.join("invocation");
    let bin = scratch.join("bin");
    fs::create_dir_all(root.join(".foundry")).unwrap();
    fs::create_dir_all(&invocation_dir).unwrap();
    fs::create_dir_all(&bin).unwrap();
    write_fake_curl(&bin.join("curl"));

    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", bin.display(), inherited_path.to_string_lossy());
    let relative_plan = Path::new("plans/new.plan.md");
    let expected_plan = root.join(relative_plan);
    let wrong_cwd_plan = invocation_dir.join(relative_plan);

    let mut child = Command::new(env!("CARGO_BIN_EXE_foundry-cli"))
        .current_dir(&invocation_dir)
        .env("PATH", test_path)
        .args([
            "propose",
            "repository-local plan regression",
            "--root",
            root.to_str().unwrap(),
            "--plan",
            relative_plan.to_str().unwrap(),
            "--db",
            root.join(".foundry/db.sqlite").to_str().unwrap(),
            "--model",
            "fake-model",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"The leased root owns the plan.\n\ny\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        expected_plan.is_file(),
        "relative --plan must create {} beneath --root",
        expected_plan.display()
    );
    assert!(
        !wrong_cwd_plan.exists(),
        "relative --plan must not create {} beneath the process CWD",
        wrong_cwd_plan.display()
    );

    fs::remove_dir_all(scratch).unwrap();
}
