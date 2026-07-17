use anyhow::{Context, Result, bail};
use foundry_core::{
    Artifact, GovernanceEnvelope, JobId, JobResult, JobState, KnowledgeLayer, RetentionPolicy,
    SourceRef, TestResult, Transformation,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::commands::common::stable_digest;
use crate::{cgroup_policy, lease, runner};

pub struct JobRunRequest {
    pub root: PathBuf,
    pub db: PathBuf,
    pub task: String,
    pub idempotency_key: Option<String>,
    pub artifacts: Vec<PathBuf>,
    pub image: String,
    pub timeout: u64,
    pub cpus: Option<u32>,
    pub memory: Option<u64>,
    pub network: bool,
    pub json: bool,
    pub workspace_baseline: Option<runner::WorkspaceSnapshot>,
    pub staged: bool,
    /// Opt this job's evidence into `DeleteAfter{now + N days}` instead of
    /// the default 30-day review policy; `foundry sweep --enforce` collects it.
    pub evidence_retention_days: Option<i64>,
    /// How the acceptance check earned authority (see
    /// `foundry_core::acceptance_authority`); `None` when the caller
    /// cannot know (direct `job-run`).
    pub acceptance_authority: Option<&'static str>,
    /// Allow silently dropping CPU/memory limits when the runtime cannot
    /// enforce them. Default (false) fails closed.
    pub allow_cgroup_fallback: bool,
    pub command: Vec<String>,
}

pub fn cmd_job_run(request: JobRunRequest) -> Result<()> {
    let mutation = lease::acquire_repository(&request.root, &lease::default_owner(), "job-run")
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    cmd_job_run_with_mutation(&mutation, request)
}

pub fn cmd_job_run_with_mutation(
    mutation: &lease::RepositoryMutation,
    request: JobRunRequest,
) -> Result<()> {
    let span = tracing::info_span!(
        "job_run",
        task_key = %request.task,
        repository = %request.root.display(),
        job_id = tracing::field::Empty,
    );
    let _entered = span.enter();
    let root = request
        .root
        .canonicalize()
        .with_context(|| format!("resolving workspace root {:?}", request.root))?;
    mutation.require_path(&root)?;
    let db_parent = request
        .db
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    mutation.require_path(db_parent)?;
    let mut graph = foundry_core::Graph::open(&request.db)
        .with_context(|| format!("opening graph at {:?}", request.db))?;
    let task_key = request.task.as_str();
    let key = request
        .idempotency_key
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{task_key}-{}", chrono::Utc::now().timestamp_micros()));
    let (job, claimed) = graph.claim_job(task_key, &key)?;
    tracing::Span::current().record("job_id", tracing::field::display(job.id.0));
    if !claimed {
        if !job.state.is_terminal() {
            bail!("idempotent job {} is still {:?}", job.id.0, job.state);
        }
        let result = graph
            .job_result(job.id)?
            .context("terminal job is missing evidence")?;
        print_job_result(&result, task_key, request.json)?;
        if result.state != JobState::Succeeded {
            bail!("container job did not succeed");
        }
        return Ok(());
    }
    let executor_image = resolve_image_identity(&request.image);
    let mut spec = foundry_core::JobSpec {
        command: request.command,
        working_directory: "/workspace".into(),
        environment: Default::default(),
        timeout_seconds: request.timeout,
        cpu_limit: request.cpus,
        memory_limit_bytes: request.memory,
        network_enabled: request.network,
    };
    let mut output = match cgroup_policy::run_podman_compatible(
        &mut spec,
        &request.image,
        &root,
        request.allow_cgroup_fallback,
    ) {
        Ok(output) => output,
        Err(error) => {
            let mut result = JobResult::new(
                job.id,
                JobState::Failed,
                runner_governance(
                    job.id,
                    &error.to_string(),
                    Vec::new(),
                    request.evidence_retention_days,
                ),
            )?;
            result.spec = Some(spec.clone());
            result.executor_image = Some(executor_image.clone());
            result.stderr = error.to_string();
            graph.finish_job(task_key, &result)?;
            return Err(error);
        }
    };
    if let Some(baseline) = &request.workspace_baseline {
        output.change_set = runner::changes_since(baseline, &root)
            .context("capturing coding-agent workspace changes")?;
    }
    let mut state = if output.cancelled {
        JobState::Cancelled
    } else if output.timed_out {
        JobState::TimedOut
    } else if output.exit_code == Some(0) {
        JobState::Succeeded
    } else {
        JobState::Failed
    };
    let mut governance = runner_governance(
        job.id,
        &format!("{}{}", output.stdout, output.stderr),
        vec![output.change_set.base_revision.clone()],
        request.evidence_retention_days,
    );
    let artifacts = match capture_artifacts(&root, &request.artifacts, &governance) {
        Ok(artifacts) => artifacts,
        Err(error) => {
            state = JobState::Failed;
            output
                .stderr
                .push_str(&format!("\nartifact capture failed: {error}"));
            governance = runner_governance(
                job.id,
                &format!("{}{}", output.stdout, output.stderr),
                vec![output.change_set.base_revision.clone()],
                request.evidence_retention_days,
            );
            Vec::new()
        }
    };
    let mut result = JobResult::new(job.id, state, governance.clone())?;
    result.spec = Some(spec.clone());
    result.exit_code = output.exit_code;
    result.stdout = output.stdout;
    result.stdout_truncated = output.stdout_truncated;
    result.stdout_dropped_bytes = output.stdout_dropped_bytes;
    result.stderr = output.stderr;
    result.stderr_truncated = output.stderr_truncated;
    result.stderr_dropped_bytes = output.stderr_dropped_bytes;
    result.duration_ms = output.duration_ms;
    result.change_set = Some(output.change_set);
    result.executor_image = Some(executor_image);
    result.acceptance_authority = request.acceptance_authority.map(str::to_string);
    result.staged = request.staged;
    if spec.command.iter().any(|part| part == "test") {
        result.tests.push(TestResult {
            command: spec.command.clone(),
            passed: state == JobState::Succeeded,
            exit_code: result.exit_code,
        });
    }
    result.artifacts = artifacts;
    graph.finish_job(task_key, &result)?;
    print_job_result(&result, task_key, request.json)?;
    if state != JobState::Succeeded {
        bail!("container job did not succeed");
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CargoTestSummary {
    pub suites: usize,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
}

pub fn cargo_test_summary(output: &str) -> Option<CargoTestSummary> {
    let mut summary = CargoTestSummary::default();
    for line in output.lines().map(str::trim) {
        if !line.starts_with("test result:") {
            continue;
        }
        summary.suites += 1;
        for segment in line.split(';') {
            let mut words = segment.split_whitespace();
            let Some(number) = words.find_map(|word| word.parse::<usize>().ok()) else {
                continue;
            };
            if segment.contains(" passed") {
                summary.passed += number;
            } else if segment.contains(" failed") {
                summary.failed += number;
            } else if segment.contains(" ignored") {
                summary.ignored += number;
            }
        }
    }
    (summary.suites > 0).then_some(summary)
}

pub fn print_job_result(result: &JobResult, task_key: &str, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(result)?);
        return Ok(());
    }

    let succeeded = result.state == JobState::Succeeded;
    let status = if succeeded { "PASSED" } else { "FAILED" };
    let command = result
        .spec
        .as_ref()
        .map(|spec| spec.command.join(" "))
        .unwrap_or_else(|| "(unknown)".into());
    println!();
    println!("Foundry verification ─────────────────────────────────────");
    println!("  Status    {status}");
    println!("  Job       {}", result.job_id.0);
    println!("  Task      {task_key}");
    println!("  Command   {command}");
    if let Some(tests) = cargo_test_summary(&result.stdout) {
        println!(
            "  Tests     {} passed · {} failed · {} ignored · {} suites",
            tests.passed, tests.failed, tests.ignored, tests.suites
        );
    } else if let Some(test) = result.tests.first() {
        println!(
            "  Tests     {} (exit {})",
            if test.passed { "passed" } else { "failed" },
            test.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".into())
        );
    }
    println!("  Duration  {:.1}s", result.duration_ms as f64 / 1000.0);
    println!(
        "  Workspace {}",
        if result.staged {
            "staged · promotion requires approval"
        } else {
            "authoritative"
        }
    );
    if let Some(spec) = &result.spec {
        let network = if spec.network_enabled { "on" } else { "off" };
        let limits = if spec.cpu_limit.is_some() || spec.memory_limit_bytes.is_some() {
            "cgroup limits on"
        } else {
            "no cgroup limits"
        };
        println!(
            "  Sandbox   network {network} · timeout {}s · {limits}",
            spec.timeout_seconds
        );
    }
    if let Some(image) = &result.executor_image {
        println!("  Image     {image}");
    }
    let changes = result
        .change_set
        .as_ref()
        .map(|change_set| change_set.files.len())
        .unwrap_or(0);
    println!(
        "  Evidence  job://{}/runner-output · {} changed files · {} artifacts",
        result.job_id.0,
        changes,
        result.artifacts.len()
    );

    if succeeded {
        println!();
        println!("Next: review the evidence");
        println!(
            "  just review-tui '{}' {} <reviewer>",
            task_key, result.job_id.0
        );
    } else {
        let diagnostics = format!("{}\n{}", result.stdout, result.stderr);
        let tail = diagnostics
            .lines()
            .filter(|line| !line.trim().is_empty())
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        if !tail.is_empty() {
            println!();
            println!("Diagnostic tail:");
            for line in tail {
                println!("  {line}");
            }
        }
    }
    println!("──────────────────────────────────────────────────────────");
    Ok(())
}

pub fn runner_governance(
    job_id: JobId,
    output: &str,
    input_digests: Vec<String>,
    retention_days: Option<i64>,
) -> GovernanceEnvelope {
    // An explicit retention opt-in makes the evidence delete-due; the
    // default remains a scheduled human review.
    let retention = match retention_days {
        Some(days) => RetentionPolicy::DeleteAfter {
            at: chrono::Utc::now() + chrono::TimeDelta::days(days),
        },
        None => RetentionPolicy::ReviewAfter {
            at: chrono::Utc::now() + chrono::TimeDelta::days(30),
        },
    };
    GovernanceEnvelope {
        layer: KnowledgeLayer::Observed,
        sources: vec![SourceRef {
            uri: format!("job://{}/runner-output", job_id.0),
            digest: Some(text_digest(output)),
        }],
        assumptions: Vec::new(),
        transformation: Transformation {
            name: "foundry-podman-runner".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            input_digests,
        },
        owner: "foundry-runner".into(),
        retention,
    }
}

fn resolve_image_identity(reference: &str) -> String {
    let output = Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Id}}", reference])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let identity = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if identity.is_empty() {
                format!("unresolved:{reference}")
            } else {
                format!("{reference}@{identity}")
            }
        }
        _ => format!("unresolved:{reference}"),
    }
}

fn text_digest(value: &str) -> String {
    stable_digest(value.as_bytes())
}

fn capture_artifacts(
    root: &Path,
    paths: &[PathBuf],
    governance: &GovernanceEnvelope,
) -> Result<Vec<Artifact>> {
    paths
        .iter()
        .map(|relative| {
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                bail!("artifact path must remain within the workspace: {relative:?}");
            }
            let path = root
                .join(relative)
                .canonicalize()
                .with_context(|| format!("resolving artifact {relative:?}"))?;
            if !path.starts_with(root) {
                bail!("artifact path escapes workspace: {relative:?}");
            }
            let bytes = fs::read(&path).with_context(|| format!("reading artifact {path:?}"))?;
            Ok(Artifact {
                name: relative
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                path: relative.to_string_lossy().into_owned(),
                digest: stable_digest(&bytes),
                size_bytes: bytes.len().try_into().unwrap_or(u64::MAX),
                governance: governance.clone(),
            })
        })
        .collect()
}
