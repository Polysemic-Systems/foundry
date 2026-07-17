//! Podman-compatible runner wrapper with cgroup-boundary policy.
//!
//! This module is separated from `runner.rs` so the core runner primitives
//! (sandbox snapshotting, stream capture, Podman invocation) stay focused and
//! the cgroup fallback policy lives in its own bounded module.

use crate::runner::RunnerOutput;
use anyhow::{Context, Result, bail};
use foundry_core::JobSpec;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Debug, PartialEq)]
pub enum CgroupLimitAction {
    Proceed,
    RetryWithoutLimits,
    Fail(String),
}

pub fn cgroup_limit_action(
    probe_output: &RunnerOutput,
    spec: &JobSpec,
    allow_cgroup_fallback: bool,
) -> CgroupLimitAction {
    if !unsupported_cgroup_limits(probe_output)
        || (spec.cpu_limit.is_none() && spec.memory_limit_bytes.is_none())
    {
        return CgroupLimitAction::Proceed;
    }
    if allow_cgroup_fallback {
        CgroupLimitAction::RetryWithoutLimits
    } else {
        CgroupLimitAction::Fail(format!(
            "Sandbox: cgroup limits (cpus={:?}, memory={:?}) could not be enforced by the container runtime. \
             Refusing to run without the requested boundary. \
             To allow fallback, rerun with --allow-cgroup-fallback or set FOUNDRY_ALLOW_CGROUP_FALLBACK=1.",
            spec.cpu_limit, spec.memory_limit_bytes
        ))
    }
}

pub fn run_podman_compatible(
    spec: &mut JobSpec,
    image: &str,
    root: &Path,
    allow_cgroup_fallback: bool,
) -> Result<RunnerOutput> {
    crate::runner::ensure_container_toolchain(spec, image, root)?;
    let action = if spec.cpu_limit.is_some() || spec.memory_limit_bytes.is_some() {
        let probe_output = probe_cgroup_limits(spec, image, root)?;
        cgroup_limit_action(&probe_output, spec, allow_cgroup_fallback)
    } else {
        CgroupLimitAction::Proceed
    };
    match action {
        CgroupLimitAction::Proceed => {}
        CgroupLimitAction::Fail(message) => bail!(message),
        CgroupLimitAction::RetryWithoutLimits => {
            eprintln!(
                "Sandbox: cgroup limits unavailable; continuing with network isolation and timeout"
            );
            spec.cpu_limit = None;
            spec.memory_limit_bytes = None;
        }
    }
    crate::runner::run_podman(spec, image, root, Arc::new(AtomicBool::new(false)))
}

/// Exercise the requested cgroup boundary with a Foundry-controlled command.
///
/// The workload is deliberately not reused as the probe: its stderr and exit
/// status are attacker-controlled and therefore cannot authorize a policy
/// downgrade.
fn probe_cgroup_limits(spec: &JobSpec, image: &str, root: &Path) -> Result<RunnerOutput> {
    let probe = JobSpec {
        command: vec!["true".into()],
        working_directory: "/workspace".into(),
        environment: Default::default(),
        timeout_seconds: 30,
        cpu_limit: spec.cpu_limit,
        memory_limit_bytes: spec.memory_limit_bytes,
        network_enabled: false,
    };
    crate::runner::run_podman(&probe, image, root, Arc::new(AtomicBool::new(false)))
}

fn unsupported_cgroup_limits(output: &RunnerOutput) -> bool {
    matches!(output.exit_code, Some(125..=127))
        && (output.stderr.contains("memory.max")
            || output.stderr.contains("cpu.max")
            || output.stderr.contains("cgroup"))
}

pub fn runner_infrastructure_failure(output: &RunnerOutput) -> bool {
    matches!(output.exit_code, Some(125..=127))
        || output.stderr.contains("OCI runtime")
        || output.stderr.contains("crun:")
}

pub fn debug_runner_preflight(root: &std::path::Path) -> Result<()> {
    let image =
        std::env::var("SANDBOX_IMAGE").unwrap_or_else(|_| "docker.io/rust:1.92-bookworm".into());
    let mut spec = JobSpec {
        command: vec!["rustc".into(), "--version".into()],
        working_directory: "/workspace".into(),
        environment: Default::default(),
        timeout_seconds: 60,
        cpu_limit: Some(2),
        memory_limit_bytes: Some(2_147_483_648),
        network_enabled: false,
    };
    let root = root.canonicalize().context("resolving workspace root")?;
    crate::runner::ensure_container_toolchain(&mut spec, &image, &root)?;
    println!("Runner debug image: {image}");
    println!("Verification runner network: disabled");
    println!("Runner debug environment: {:?}", spec.environment);
    println!(
        "Runner debug command: {:?}",
        spec.podman_args(&image, &root.to_string_lossy(), "foundry-preflight")
    );
    let output = run_podman_compatible(&mut spec, &image, &root, false)?;
    println!(
        "Runner preflight: exit={:?}, stdout={:?}, stderr={:?}",
        output.exit_code,
        output.stdout.trim(),
        output.stderr.trim()
    );
    if output.exit_code != Some(0) {
        bail!("runner preflight failed before durable job creation");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunnerOutput;
    use foundry_core::{ChangeSet, JobId, JobSpec};
    use std::{fs, path::Path};

    // This fixture represents a trusted, Foundry-controlled runtime probe.
    // Workload output that happens to contain the same text is covered by the
    // process-level regression test below and must not reach this policy input.
    fn unsupported_limits_probe_output() -> RunnerOutput {
        RunnerOutput {
            exit_code: Some(125),
            stdout: String::new(),
            stdout_truncated: false,
            stdout_dropped_bytes: 0,
            stderr: "crun: opening `/sys/fs/cgroup/memory.max` failed".into(),
            stderr_truncated: false,
            stderr_dropped_bytes: 0,
            duration_ms: 0,
            timed_out: false,
            cancelled: false,
            change_set: ChangeSet {
                base_revision: "sha256:base".into(),
                patch_digest: "sha256:patch".into(),
                files: Vec::new(),
            },
        }
    }

    #[test]
    fn cgroup_fallback_is_prohibited_by_default() {
        let output = unsupported_limits_probe_output();
        let spec = JobSpec {
            command: vec!["cargo".into(), "test".into()],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            timeout_seconds: 300,
            cpu_limit: Some(2),
            memory_limit_bytes: Some(2_147_483_648),
            network_enabled: false,
        };
        let action = cgroup_limit_action(&output, &spec, false);
        assert!(
            matches!(action, CgroupLimitAction::Fail(ref msg) if msg.contains("Refusing to run")),
            "default must fail closed: {action:?}"
        );
    }

    #[test]
    fn cgroup_fallback_is_allowed_when_explicitly_consented() {
        let output = unsupported_limits_probe_output();
        let spec = JobSpec {
            command: vec!["cargo".into(), "test".into()],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            timeout_seconds: 300,
            cpu_limit: Some(2),
            memory_limit_bytes: Some(2_147_483_648),
            network_enabled: false,
        };
        assert_eq!(
            cgroup_limit_action(&output, &spec, true),
            CgroupLimitAction::RetryWithoutLimits,
            "explicit consent must allow fallback"
        );
    }

    #[test]
    fn cgroup_error_without_requested_limits_proceeds() {
        let output = unsupported_limits_probe_output();
        let spec = JobSpec {
            command: vec!["cargo".into(), "test".into()],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            timeout_seconds: 300,
            cpu_limit: None,
            memory_limit_bytes: None,
            network_enabled: false,
        };
        assert_eq!(
            cgroup_limit_action(&output, &spec, false),
            CgroupLimitAction::Proceed,
            "limits were not requested, so unsupported runtime limits are irrelevant"
        );
    }

    #[test]
    fn normal_failure_proceeds_without_cgroup_fallback() {
        let output = RunnerOutput {
            exit_code: Some(1),
            stdout: String::new(),
            stdout_truncated: false,
            stdout_dropped_bytes: 0,
            stderr: "error: test failed".into(),
            stderr_truncated: false,
            stderr_dropped_bytes: 0,
            duration_ms: 0,
            timed_out: false,
            cancelled: false,
            change_set: ChangeSet {
                base_revision: "sha256:base".into(),
                patch_digest: "sha256:patch".into(),
                files: Vec::new(),
            },
        };
        let spec = JobSpec {
            command: vec!["cargo".into(), "test".into()],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            timeout_seconds: 300,
            cpu_limit: Some(2),
            memory_limit_bytes: Some(2_147_483_648),
            network_enabled: false,
        };
        assert_eq!(
            cgroup_limit_action(&output, &spec, false),
            CgroupLimitAction::Proceed,
            "non-cgroup failures must not trigger fallback logic"
        );
    }

    #[cfg(unix)]
    #[test]
    fn forged_workload_diagnostics_cannot_trigger_resource_downgrades() {
        const CHILD_ROOT: &str = "FOUNDRY_CGROUP_REGRESSION_CHILD_ROOT";
        const PODMAN_LOG: &str = "FOUNDRY_CGROUP_REGRESSION_PODMAN_LOG";

        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            exercise_forged_workload_diagnostics(
                Path::new(&root),
                Path::new(&std::env::var_os(PODMAN_LOG).expect("child Podman log path")),
            );
            return;
        }

        // PATH is process-global, so run the assertion in a child test process
        // whose environment can safely point `podman` at the deterministic
        // fake below without racing other tests.
        let fixture =
            std::env::temp_dir().join(format!("foundry-cgroup-workload-output-{}", JobId::new().0));
        let bin = fixture.join("bin");
        let workspace = fixture.join("workspace");
        let log = fixture.join("podman.log");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let fake_podman = bin.join("podman");
        fs::write(
            &fake_podman,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$FOUNDRY_CGROUP_REGRESSION_PODMAN_LOG"
case "$*" in
  *forged-memory-max*)
    printf '%s\n' 'crun: opening `/sys/fs/cgroup/memory.max` failed' >&2
    exit 125
    ;;
  *forged-cpu-max*)
    printf '%s\n' 'crun: opening `/sys/fs/cgroup/cpu.max` failed' >&2
    exit 126
    ;;
  *forged-cgroup*)
    printf '%s\n' 'OCI runtime error: cgroup unavailable' >&2
    exit 127
    ;;
  *)
    # Any Foundry-controlled capability probe succeeds. Only the workload
    # commands above emit attacker-controlled diagnostics.
    exit 0
    ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_podman, fs::Permissions::from_mode(0o755)).unwrap();

        let mut search_path = vec![bin];
        if let Some(path) = std::env::var_os("PATH") {
            search_path.extend(std::env::split_paths(&path));
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("forged_workload_diagnostics_cannot_trigger_resource_downgrades")
            .arg("--nocapture")
            .env("PATH", std::env::join_paths(search_path).unwrap())
            .env(CHILD_ROOT, &workspace)
            .env(PODMAN_LOG, &log)
            .status()
            .unwrap();

        fs::remove_dir_all(&fixture).unwrap();
        assert!(
            status.success(),
            "the isolated malicious-workload regression failed"
        );
    }

    #[cfg(unix)]
    fn exercise_forged_workload_diagnostics(root: &Path, podman_log: &Path) {
        let attacks = [
            ("forged-memory-max", 125),
            ("forged-cpu-max", 126),
            ("forged-cgroup", 127),
        ];
        let mut observations = Vec::new();

        for (command, exit_code) in attacks {
            let mut spec = JobSpec {
                command: vec![command.into()],
                working_directory: "/workspace".into(),
                environment: Default::default(),
                timeout_seconds: 30,
                cpu_limit: Some(2),
                memory_limit_bytes: Some(2_147_483_648),
                network_enabled: false,
            };

            let output = run_podman_compatible(&mut spec, "trusted-image", root, true).unwrap();
            observations.push((command, exit_code, output.exit_code, spec));
        }

        let invocations = fs::read_to_string(podman_log).unwrap();
        for (command, expected_exit, actual_exit, spec) in observations {
            let workload_invocations = invocations
                .lines()
                .filter(|line| line.split_whitespace().any(|arg| arg == command))
                .collect::<Vec<_>>();
            assert_eq!(
                workload_invocations.len(),
                1,
                "workload-controlled output must not cause {command} to be retried without limits:\n{invocations}"
            );
            let invocation = workload_invocations[0];
            assert!(
                invocation.contains("--cpus 2"),
                "the workload lost its CPU boundary: {invocation}"
            );
            assert!(
                invocation.contains("--memory 2147483648"),
                "the workload lost its memory boundary: {invocation}"
            );
            assert_eq!(actual_exit, Some(expected_exit));
            assert_eq!(spec.cpu_limit, Some(2), "CPU policy was downgraded");
            assert_eq!(
                spec.memory_limit_bytes,
                Some(2_147_483_648),
                "memory policy was downgraded"
            );
        }
    }
}
