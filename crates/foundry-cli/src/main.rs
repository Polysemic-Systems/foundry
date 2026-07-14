use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use foundry_core::{
    Artifact, Event, GovernanceEnvelope, JobId, JobResult, JobState, KnowledgeLayer,
    RetentionPolicy, Review, ReviewDecision, RuleResult, SourceRef, TaskState, TestResult,
    Transformation,
    graph::{Graph, NodeKind},
    plan::Plan,
    sanitize_query,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use walkdir::WalkDir;

mod runner;

#[derive(Parser)]
#[command(name = "foundry")]
#[command(about = "A local-first, self-building production system.")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a command in a bounded, non-interactive Podman container.
    JobRun {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "docker.io/rust:1-bookworm")]
        image: String,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        #[arg(long)]
        cpus: Option<u32>,
        #[arg(long)]
        memory: Option<u64>,
        #[arg(long)]
        network: bool,
        /// Durable graph receiving lifecycle state and evidence.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Stable plan-relative key of the originating task.
        #[arg(long)]
        task: String,
        /// Retry-safe operation key. Defaults to a fresh attempt.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Workspace-relative artifact path to capture; repeatable.
        #[arg(long)]
        artifact: Vec<PathBuf>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Approve successful job evidence and complete its task.
    ReviewApprove {
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        #[arg(long)]
        task: String,
        #[arg(long)]
        job: uuid::Uuid,
        #[arg(long)]
        reviewer: String,
        #[arg(long)]
        reason: String,
    },
    /// Reject successful job evidence and return its task to ready.
    ReviewReject {
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        #[arg(long)]
        task: String,
        #[arg(long)]
        job: uuid::Uuid,
        #[arg(long)]
        reviewer: String,
        #[arg(long)]
        reason: String,
    },
    /// Initialize a foundry workspace.
    Init {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Index the codebase into the production graph.
    Index {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Generate and store Ollama embeddings for semantic search.
        #[arg(long)]
        embed: bool,
    },
    /// Show the bootstrap plan.
    Plan,
    /// List nodes in the graph.
    List {
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Filter by node kind.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Search indexed code.
    Search {
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Query.
        query: String,
    },
    /// Semantic code search using dense embeddings.
    #[command(name = "semsearch")]
    SemSearch {
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Ollama embedding model to use.
        #[arg(long, default_value = "nomic-embed-text:latest")]
        model: String,
        /// How many results to return.
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Query.
        query: String,
    },
    /// Rebuild the graph from source files.
    Rebuild {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Generate and store Ollama embeddings for semantic search.
        #[arg(long)]
        embed: bool,
    },
    /// Compare graph state to filesystem and report drift.
    Reconcile {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
    },
    /// Detect drift and rebuild the graph if needed.
    Heal {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
    },
    /// Run self-diagnostic checks.
    Doctor {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Path to the plan file.
        #[arg(long, default_value = "./plans/bootstrap.plan.md")]
        plan: PathBuf,
    },
    /// Run rule-based diagnostics on the graph.
    CheckRules {
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
    },
    /// Approve a rule so it can run in check-rules.
    ApproveRule {
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Rule ID to approve.
        rule_id: String,
    },
    /// Ask a local model about the codebase, using the graph as RAG context.
    Ask {
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Ollama model to use.
        #[arg(long, default_value = "gemma4:e2b")]
        model: String,
        /// How many code snippets to retrieve for context.
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// The question.
        query: String,
    },
    /// Execute the next undone task in the plan.
    Iterate {
        /// Path to the plan file.
        #[arg(long, default_value = "./plans/bootstrap.plan.md")]
        plan: PathBuf,
        /// Project root (for commands that need it).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
    },
    /// Propose a new feature. Foundry discusses it with you, then appends approved tasks to the plan.
    Propose {
        /// Short description of the feature.
        query: Option<String>,
        /// Path to the plan file.
        #[arg(long, default_value = "./plans/features.plan.md")]
        plan: PathBuf,
        /// Project root (for commands that need it).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Ollama model to use.
        #[arg(long, default_value = "gemma4:e2b")]
        model: String,
    },
    /// Create, list, and restore database snapshots.
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
}

#[derive(Subcommand)]
enum SnapshotAction {
    /// Create a snapshot of the database.
    Create {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Snapshot name. Defaults to an ISO-8601 timestamp.
        name: Option<String>,
    },
    /// List available snapshots.
    List {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
    },
    /// Restore a snapshot over the current database.
    Restore {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Snapshot name to restore.
        name: String,
        /// Skip confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::JobRun {
            root,
            image,
            timeout,
            cpus,
            memory,
            network,
            db,
            task,
            idempotency_key,
            artifact,
            command,
        } => cmd_job_run(JobRunRequest {
            root,
            db,
            task,
            idempotency_key,
            artifacts: artifact,
            image,
            timeout,
            cpus,
            memory,
            network,
            command,
        }),
        Commands::ReviewApprove {
            db,
            task,
            job,
            reviewer,
            reason,
        } => cmd_review(
            &db,
            &task,
            JobId(job),
            ReviewDecision::Approve,
            &reviewer,
            &reason,
        ),
        Commands::ReviewReject {
            db,
            task,
            job,
            reviewer,
            reason,
        } => cmd_review(
            &db,
            &task,
            JobId(job),
            ReviewDecision::Reject,
            &reviewer,
            &reason,
        ),
        Commands::Init { root } => cmd_init(&root),
        Commands::Index { root, db, embed } => cmd_index(&root, &db, embed),
        Commands::Plan => cmd_plan(),
        Commands::List { db, kind } => cmd_list(&db, kind.as_deref()),
        Commands::Search { db, query } => cmd_search(&db, &query),
        Commands::SemSearch {
            db,
            model,
            limit,
            query,
        } => cmd_semsearch(&db, &model, &query, limit),
        Commands::Rebuild { root, db, embed } => cmd_rebuild(&root, &db, embed),
        Commands::Reconcile { root, db } => cmd_reconcile(&root, &db),
        Commands::Heal { root, db } => cmd_heal(&root, &db),
        Commands::Doctor { root, db, plan } => cmd_doctor(&root, &db, &plan),
        Commands::CheckRules { db } => cmd_check_rules(&db),
        Commands::ApproveRule { db, rule_id } => cmd_approve_rule(&db, &rule_id),
        Commands::Ask {
            db,
            model,
            limit,
            query,
        } => cmd_ask(&db, &model, &query, limit),
        Commands::Iterate { plan, root, db } => cmd_iterate(&plan, &root, &db),
        Commands::Propose {
            query,
            plan,
            root,
            db,
            model,
        } => cmd_propose(query.as_deref(), &plan, &root, &db, &model),
        Commands::Snapshot { action } => cmd_snapshot(action),
    }
}

fn cmd_review(
    db: &Path,
    task_key: &str,
    job_id: JobId,
    decision: ReviewDecision,
    reviewer: &str,
    reason: &str,
) -> Result<()> {
    if reviewer.trim().is_empty() || reason.trim().is_empty() {
        bail!("reviewer and reason must be non-empty");
    }
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {db:?}"))?;
    let review = Review {
        task_key: task_key.into(),
        job_id,
        decision,
        reviewer: reviewer.into(),
        reason: reason.into(),
    };
    let state = graph.record_review(&review)?;
    println!(
        "{}",
        serde_json::json!({ "task": task_key, "state": state.as_str(), "review": review })
    );
    Ok(())
}

struct JobRunRequest {
    root: PathBuf,
    db: PathBuf,
    task: String,
    idempotency_key: Option<String>,
    artifacts: Vec<PathBuf>,
    image: String,
    timeout: u64,
    cpus: Option<u32>,
    memory: Option<u64>,
    network: bool,
    command: Vec<String>,
}

fn cmd_job_run(request: JobRunRequest) -> Result<()> {
    let root = request
        .root
        .canonicalize()
        .with_context(|| format!("resolving workspace root {:?}", request.root))?;
    let mut graph =
        Graph::open(&request.db).with_context(|| format!("opening graph at {:?}", request.db))?;
    let task_key = request.task.as_str();
    let task_state = graph.initialize_task_state(task_key, TaskState::Ready)?;
    if task_state != TaskState::Ready {
        bail!("task {task_key} is {task_state:?}, expected ready");
    }
    let key = request
        .idempotency_key
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{task_key}-{}", chrono::Utc::now().timestamp_micros()));
    let job = graph.create_job(task_key, &key)?;
    if job.state != JobState::Queued {
        if !job.state.is_terminal() {
            bail!("idempotent job {} is still {:?}", job.id.0, job.state);
        }
        let result = graph
            .job_result(job.id)?
            .context("terminal job is missing evidence")?;
        println!("{}", serde_json::to_string(&result)?);
        if result.state != JobState::Succeeded {
            bail!("container job did not succeed");
        }
        return Ok(());
    }
    graph.transition_task(task_key, TaskState::Running)?;
    graph.transition_job(job.id, JobState::Running)?;
    let spec = foundry_core::JobSpec {
        command: request.command,
        working_directory: "/workspace".into(),
        environment: Default::default(),
        timeout_seconds: request.timeout,
        cpu_limit: request.cpus,
        memory_limit_bytes: request.memory,
        network_enabled: request.network,
    };
    let mut output = match runner::run_podman(
        &spec,
        &request.image,
        &root,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    ) {
        Ok(output) => output,
        Err(error) => {
            graph.transition_job(job.id, JobState::Failed)?;
            let mut result = JobResult::new(
                job.id,
                JobState::Failed,
                runner_governance(job.id, &error.to_string(), Vec::new()),
            )?;
            result.spec = Some(spec.clone());
            result.stderr = error.to_string();
            graph.record_job_result(&result)?;
            graph.transition_task(task_key, TaskState::Failed)?;
            return Err(error);
        }
    };
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
            );
            Vec::new()
        }
    };
    graph.transition_job(job.id, state)?;
    let mut result = JobResult::new(job.id, state, governance.clone())?;
    result.spec = Some(spec.clone());
    result.exit_code = output.exit_code;
    result.stdout = output.stdout;
    result.stderr = output.stderr;
    result.duration_ms = output.duration_ms;
    result.change_set = Some(output.change_set);
    if spec.command.iter().any(|part| part == "test") {
        result.tests.push(TestResult {
            command: spec.command.clone(),
            passed: state == JobState::Succeeded,
            exit_code: result.exit_code,
        });
    }
    result.artifacts = artifacts;
    graph.record_job_result(&result)?;
    graph.transition_task(
        task_key,
        if state == JobState::Succeeded {
            TaskState::Review
        } else {
            TaskState::Failed
        },
    )?;
    println!("{}", serde_json::to_string(&result)?);
    if state != JobState::Succeeded {
        bail!("container job did not succeed");
    }
    Ok(())
}

fn runner_governance(
    job_id: JobId,
    output: &str,
    input_digests: Vec<String>,
) -> GovernanceEnvelope {
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
        retention: RetentionPolicy::ReviewAfter {
            at: chrono::Utc::now() + chrono::TimeDelta::days(30),
        },
    }
}

fn text_digest(value: &str) -> String {
    stable_digest(value.as_bytes())
}

fn stable_digest(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}")
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

fn cmd_init(root: &Path) -> Result<()> {
    let foundry_dir = root.join(".foundry");
    fs::create_dir_all(&foundry_dir).with_context(|| format!("creating {:?}", foundry_dir))?;

    let db_path = foundry_dir.join("db.sqlite");
    Graph::open(&db_path).with_context(|| format!("opening graph at {:?}", db_path))?;

    println!(
        "Initialized foundry at {:?}",
        root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
    );
    println!("Database: {:?}", db_path);
    Ok(())
}

/// Character budget sent to the embedding model. Keep it below the model's context
/// length (nomic-embed-text supports ~2048 tokens).
const EMBED_MAX_CHARS: usize = 4000;

fn cmd_index(root: &Path, db: &Path, embed: bool) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    // Collect text files first so code search is populated before plan linking.
    let mut text_files = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_ignored(e.path()))
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // skip binary files
        };
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        text_files.push((relative, content));
    }

    let mut indexed = 0;
    let mut embedded = 0;
    let mut plans = Vec::new();
    let mut embed_available = embed;
    let embed_model = "nomic-embed-text:latest";

    for (relative, content) in text_files {
        if relative.ends_with(".plan.md") {
            plans.push((relative, content));
            continue;
        }

        let embedding = if embed_available {
            let embed_text: String = content.chars().take(EMBED_MAX_CHARS).collect();
            match embed_ollama(embed_model, &embed_text) {
                Ok(emb) => {
                    embedded += 1;
                    Some(emb)
                }
                Err(e) => {
                    eprintln!(
                        "warning: embedding failed for {} ({}); continuing without embeddings",
                        relative, e
                    );
                    embed_available = false;
                    None
                }
            }
        } else {
            None
        };

        if let Some(ref emb) = embedding {
            graph
                .index_code_with_embedding(&relative, &content, Some(emb))
                .with_context(|| format!("indexing {}", relative))?;
        } else {
            graph
                .index_code(&relative, &content)
                .with_context(|| format!("indexing {}", relative))?;
        }
        indexed += 1;
    }

    for (relative, content) in plans {
        let plan = Plan::parse(&relative, &content);
        graph
            .index_plan(&relative, &plan)
            .with_context(|| format!("indexing plan {}", relative))?;
        indexed += 1;
    }

    if embed {
        println!(
            "Indexed {} files ({} with embeddings) into {:?}",
            indexed, embedded, db
        );
    } else {
        println!("Indexed {} files into {:?}", indexed, db);
    }
    Ok(())
}

fn cmd_plan() -> Result<()> {
    let plan_path = Path::new("./plans/bootstrap.plan.md");
    let plan_text = fs::read_to_string(plan_path)
        .with_context(|| format!("reading plan at {:?}", plan_path))?;
    let plan = Plan::parse("bootstrap", &plan_text);
    print!("{}", plan);
    Ok(())
}

fn cmd_list(db: &Path, kind: Option<&str>) -> Result<()> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    let kind_filter = kind.and_then(|k| k.parse::<NodeKind>().ok());

    let nodes = graph.list_nodes(kind_filter).context("listing nodes")?;
    for node in nodes {
        println!("{}\t{}\t{}", node.id.0, node.kind.as_str(), node.name);
    }
    Ok(())
}

fn cmd_search(db: &Path, query: &str) -> Result<()> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    let safe = sanitize_query(query);
    let results = graph.search_code(&safe).context("searching code")?;
    for (node, _content) in results {
        println!("{}\t{}", node.id.0, node.name);
    }
    Ok(())
}

fn cmd_rebuild(root: &Path, db: &Path, embed: bool) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    graph
        .truncate_derived()
        .context("truncating derived state")?;
    println!("Truncated derived state. Re-indexing...");
    cmd_index(root, db, embed)
}

fn cmd_reconcile(root: &Path, db: &Path) -> Result<()> {
    let (in_sync, missing_on_disk, not_in_graph) = reconcile_state(root, db)?;

    if in_sync {
        println!("Graph and filesystem are in sync.");
        return Ok(());
    }

    if !missing_on_disk.is_empty() {
        println!("In graph but missing on disk ({}):", missing_on_disk.len());
        for path in missing_on_disk {
            println!("  - {}", path);
        }
    }
    if !not_in_graph.is_empty() {
        println!("On disk but not in graph ({}):", not_in_graph.len());
        for path in not_in_graph {
            println!("  + {}", path);
        }
    }

    Ok(())
}

fn reconcile_state(root: &Path, db: &Path) -> Result<(bool, Vec<String>, Vec<String>)> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    let code_nodes = graph
        .list_nodes(Some(NodeKind::Code))
        .context("listing code nodes")?;
    let in_graph: std::collections::HashSet<String> =
        code_nodes.into_iter().map(|n| n.name).collect();

    let mut on_disk = std::collections::HashSet::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_ignored(e.path()))
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        if fs::read_to_string(entry.path()).is_err() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();
        // Plan files are represented as Plan nodes, not Code nodes.
        if relative.ends_with(".plan.md") {
            continue;
        }
        on_disk.insert(relative);
    }

    let missing_on_disk: Vec<String> = in_graph.difference(&on_disk).cloned().collect();
    let not_in_graph: Vec<String> = on_disk.difference(&in_graph).cloned().collect();
    let in_sync = missing_on_disk.is_empty() && not_in_graph.is_empty();

    Ok((in_sync, missing_on_disk, not_in_graph))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Ok,
    Warn,
    Fail,
}

struct Check {
    name: String,
    health: Health,
    message: String,
}

fn ok(name: impl Into<String>, message: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        health: Health::Ok,
        message: message.into(),
    }
}

fn warn(name: impl Into<String>, message: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        health: Health::Warn,
        message: message.into(),
    }
}

fn fail(name: impl Into<String>, message: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        health: Health::Fail,
        message: message.into(),
    }
}

fn cmd_doctor(root: &Path, db: &Path, plan_path: &Path) -> Result<()> {
    const EXPECTED_SCHEMA_VERSION: i64 = foundry_core::graph::LATEST_SCHEMA_VERSION;

    let mut checks = Vec::new();

    // 1. Database can be opened.
    let graph = match Graph::open(db) {
        Ok(g) => {
            checks.push(ok("db_open", "graph database opened"));
            g
        }
        Err(e) => {
            checks.push(fail("db_open", format!("cannot open database: {}", e)));
            print_report(&checks);
            return Ok(());
        }
    };

    // 2. WAL mode.
    match graph.wal_mode() {
        Ok(true) => checks.push(ok("wal_mode", "WAL journaling enabled")),
        Ok(false) => checks.push(warn("wal_mode", "WAL journaling not enabled")),
        Err(e) => checks.push(fail("wal_mode", format!("cannot read journal mode: {}", e))),
    }

    // 3. Schema version.
    match graph.schema_version() {
        Ok(v) if v == EXPECTED_SCHEMA_VERSION => {
            checks.push(ok("schema_version", format!("version {}", v)));
        }
        Ok(v) => checks.push(warn(
            "schema_version",
            format!("version {} (expected {})", v, EXPECTED_SCHEMA_VERSION),
        )),
        Err(e) => checks.push(fail("schema_version", format!("cannot read: {}", e))),
    }

    // 4. Graph in sync with filesystem.
    match reconcile_state(root, db) {
        Ok((true, _, _)) => checks.push(ok("filesystem_sync", "graph and filesystem in sync")),
        Ok((false, missing, extra)) => {
            let msg = format!(
                "drift: {} missing on disk, {} not in graph",
                missing.len(),
                extra.len()
            );
            checks.push(fail("filesystem_sync", msg));
        }
        Err(e) => checks.push(fail("filesystem_sync", format!("cannot reconcile: {}", e))),
    }

    // 5. Plan parseable.
    match fs::read_to_string(plan_path) {
        Ok(text) => match Plan::parse("bootstrap", &text).first_undone() {
            Some(task) if task.stop.is_some() => checks.push(warn(
                "plan_state",
                format!(
                    "next task '{}' requires human stop approval",
                    task.description
                ),
            )),
            Some(_) => checks.push(ok("plan_state", "plan has runnable next task")),
            None => checks.push(ok("plan_state", "plan complete")),
        },
        Err(e) => checks.push(fail("plan_state", format!("cannot read plan: {}", e))),
    }

    // 6. Events recorded.
    match graph.events(1) {
        Ok(events) if !events.is_empty() => checks.push(ok("events", "events table has rows")),
        Ok(_) => checks.push(warn("events", "events table is empty")),
        Err(e) => checks.push(fail("events", format!("cannot read events: {}", e))),
    }

    // 7. Required tools on PATH.
    for tool in ["cargo", "just"] {
        if Command::new(tool).arg("--version").output().is_ok() {
            checks.push(ok(format!("tool_{}", tool), "found on PATH"));
        } else {
            checks.push(fail(format!("tool_{}", tool), "not found on PATH"));
        }
    }
    // 8. Optional local-model / network / sandbox tools.
    for tool in ["ollama", "curl", "podman"] {
        if Command::new(tool).arg("--version").output().is_ok() {
            checks.push(ok(format!("tool_{}", tool), "found on PATH"));
        } else {
            checks.push(warn(format!("tool_{}", tool), "not found on PATH"));
        }
    }

    print_report(&checks);

    let failures = checks.iter().filter(|c| c.health == Health::Fail).count();
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn print_report(checks: &[Check]) {
    for check in checks {
        let symbol = match check.health {
            Health::Ok => "[OK]",
            Health::Warn => "[WARN]",
            Health::Fail => "[FAIL]",
        };
        println!("{} {}: {}", symbol, check.name, check.message);
    }
}

fn cmd_check_rules(db: &Path) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    let rules = foundry_core::built_in_rules();

    // Upsert rules and detect any that are not yet approved.
    let unapproved = foundry_core::rules::upsert_rules(&mut graph, &rules)
        .with_context(|| "upserting rules into graph")?;

    if !unapproved.is_empty() {
        for rule_id in &unapproved {
            if let Some(node) = graph.find_node_by_name(NodeKind::Rule, rule_id)? {
                graph
                    .record_event(&Event::ReviewRequested {
                        review_id: node.id,
                        task_id: node.id,
                    })
                    .with_context(|| format!("recording review request for {}", rule_id))?;
            }
        }
        eprintln!("Review required for new/unapproved rules:");
        for rule_id in &unapproved {
            if let Ok(Some(node)) = graph.find_node_by_name(NodeKind::Rule, rule_id) {
                let name = node
                    .payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(rule_id);
                eprintln!("  - {} ({})", rule_id, name);
            } else {
                eprintln!("  - {}", rule_id);
            }
        }
        eprintln!(
            "\nApprove each with: foundry approve-rule <rule-id> --db {:?}",
            db
        );
        bail!(
            "{} rule(s) pending approval; check-rules blocked.",
            unapproved.len()
        );
    }

    let mut failures = 0;
    let mut warnings = 0;

    for rule in &rules {
        let rule_result = match rule.check(&graph) {
            Ok(rr) => rr,
            Err(e) => RuleResult::Fail {
                reason: format!("rule engine error: {}", e),
            },
        };

        let rule_node = graph
            .find_node_by_name(NodeKind::Rule, rule.id())
            .with_context(|| format!("finding rule node for {}", rule.id()))?;
        let rule_id = match rule_node {
            Some(n) => n.id,
            None => {
                bail!("rule node {} missing after upsert", rule.id());
            }
        };
        graph
            .record_event(&Event::RuleTriggered {
                rule_id,
                result: rule_result.clone(),
            })
            .with_context(|| format!("recording rule event for {}", rule.id()))?;

        match rule_result {
            RuleResult::Pass => println!("[PASS] {} ({})", rule.name(), rule.id()),
            RuleResult::Warn { reason } => {
                println!("[WARN] {} ({}): {}", rule.name(), rule.id(), reason);
                warnings += 1;
            }
            RuleResult::Fail { reason } => {
                println!("[FAIL] {} ({}): {}", rule.name(), rule.id(), reason);
                failures += 1;
            }
        }
    }

    if failures > 0 {
        bail!("{} rule(s) failed", failures);
    }
    if warnings > 0 {
        println!("\n{} warning(s); no failures.", warnings);
    } else {
        println!("\nAll rules passed.");
    }
    Ok(())
}

fn cmd_approve_rule(db: &Path, rule_id: &str) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    match foundry_core::rules::approve_rule(&mut graph, rule_id) {
        Ok(true) => {
            println!("Approved rule: {}", rule_id);
            Ok(())
        }
        Ok(false) => {
            bail!(
                "Rule '{}' not found in graph. Run `foundry check-rules` first to register it.",
                rule_id
            );
        }
        Err(e) => {
            bail!("Failed to approve rule '{}': {}", rule_id, e);
        }
    }
}

fn cmd_ask(db: &Path, model: &str, query: &str, limit: usize) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    let safe_query = sanitize_query(query);
    let results = graph
        .search_code(&safe_query)
        .context("searching graph for context")?;

    if results.is_empty() {
        println!(
            "No indexed code matched '{}'. Try a more specific term.",
            safe_query
        );
        return Ok(());
    }

    let mut context = String::new();
    for (i, (node, content)) in results.iter().take(limit).enumerate() {
        context.push_str(&format!(
            "--- snippet {}: {} ---\n{}\n\n",
            i + 1,
            node.name,
            content.chars().take(2000).collect::<String>()
        ));
    }

    let prompt = format!(
        "Use the following code snippets from the codebase to answer the question.\n\n{}\nQuestion: {}\nAnswer:",
        context, query
    );

    let messages = vec![ChatMessage {
        role: "user",
        content: prompt.as_str(),
    }];
    let (answer, prompt_tokens, _completion_tokens) =
        ask_ollama(model, &messages).with_context(|| format!("asking model {}", model))?;

    graph
        .record_event(&Event::ModelInvoked {
            model: model.to_string(),
            prompt_tokens,
            cost_usd: 0.0,
        })
        .context("recording model invocation")?;

    println!("{}", answer);
    Ok(())
}

struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Call a local Ollama model with a chat history.
/// Returns the assistant's reply plus token counts.
fn ask_ollama(model: &str, messages: &[ChatMessage<'_>]) -> Result<(String, u64, u64)> {
    let payload_messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
        .collect();
    let payload = serde_json::json!({
        "model": model,
        "messages": payload_messages,
        "stream": false,
    });

    let mut child = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "http://localhost:11434/api/chat",
            "-H",
            "Content-Type: application/json",
            "-d",
            "@-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning curl to call Ollama. Is Ollama running on localhost:11434?")?;

    {
        let mut stdin = child.stdin.take().context("opening curl stdin")?;
        stdin
            .write_all(payload.to_string().as_bytes())
            .context("writing prompt to curl")?;
    }

    let output = child.wait_with_output().context("waiting for curl")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("curl failed: {}", stderr);
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing Ollama response")?;
    if let Some(error) = json.get("error").and_then(|v| v.as_str()) {
        bail!("Ollama error: {}", error);
    }
    let answer = json
        .get("message")
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let prompt_tokens = json
        .get("prompt_eval_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion_tokens = json.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0);

    Ok((answer, prompt_tokens, completion_tokens))
}

fn cmd_semsearch(db: &Path, model: &str, query: &str, limit: usize) -> Result<()> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    let query_embedding =
        embed_ollama(model, query).with_context(|| format!("embedding query with {}", model))?;

    let results = graph
        .semantic_search(&query_embedding, limit)
        .context("running semantic search")?;

    if results.is_empty() {
        println!(
            "No embeddings found. Run `foundry index --embed` or `foundry rebuild --embed` first."
        );
        return Ok(());
    }

    for (node, score) in results {
        println!("{:.4}\t{}", score, node.name);
    }
    Ok(())
}

fn embed_ollama(model: &str, text: &str) -> Result<Vec<f32>> {
    let payload = serde_json::json!({
        "model": model,
        "prompt": text,
    });

    let mut child = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "http://localhost:11434/api/embeddings",
            "-H",
            "Content-Type: application/json",
            "-d",
            "@-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(
            "spawning curl to call Ollama embeddings. Is Ollama running on localhost:11434?",
        )?;

    {
        let mut stdin = child.stdin.take().context("opening curl stdin")?;
        stdin
            .write_all(payload.to_string().as_bytes())
            .context("writing prompt to curl")?;
    }

    let output = child.wait_with_output().context("waiting for curl")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("curl failed: {}", stderr);
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing Ollama embeddings response")?;
    if let Some(error) = json.get("error").and_then(|v| v.as_str()) {
        bail!("Ollama embeddings error: {}", error);
    }
    let embedding = json
        .get("embedding")
        .and_then(|v| v.as_array())
        .context("missing embedding array in response")?
        .iter()
        .map(|v| {
            v.as_f64()
                .map(|f| f as f32)
                .context("non-numeric embedding value")
        })
        .collect::<Result<Vec<_>, _>>()
        .context("embedding values")?;

    Ok(embedding)
}

fn cmd_heal(root: &Path, db: &Path) -> Result<()> {
    let (in_sync, _, _) = reconcile_state(root, db).context("initial reconcile")?;
    if in_sync {
        println!("Healthy: graph and filesystem are in sync.");
        return Ok(());
    }

    println!("Drift detected. Rebuilding graph from source...");
    cmd_rebuild(root, db, false)?;

    let (in_sync_after, missing, extra) = reconcile_state(root, db).context("verify reconcile")?;
    if in_sync_after {
        println!("Healed: graph is now in sync with filesystem.");
        Ok(())
    } else {
        eprintln!("Heal failed: drift remains after rebuild.");
        for path in missing {
            eprintln!("  missing on disk: {}", path);
        }
        for path in extra {
            eprintln!("  not in graph: {}", path);
        }
        bail!("self-heal could not resolve drift");
    }
}

fn cmd_iterate(plan_path: &Path, root: &Path, db: &Path) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    let plan_text = fs::read_to_string(plan_path)
        .with_context(|| format!("reading plan at {:?}", plan_path))?;
    let mut plan = Plan::parse("bootstrap", &plan_text);

    // Index the plan so task nodes exist in the graph and are linked to code.
    let plan_relative = plan_path
        .strip_prefix(root)
        .unwrap_or(plan_path)
        .to_string_lossy()
        .to_string();
    graph
        .index_plan(&plan_relative, &plan)
        .with_context(|| format!("indexing plan {}", plan_relative))?;

    // An approval is authoritative. Reflect previously approved task state in
    // markdown before selecting the next runnable task.
    let mut synchronized = false;
    for task in plan.tasks.clone() {
        let task_key = format!("{}#{}", plan_relative, task.id);
        if !task.done && graph.task_state(&task_key)? == Some(TaskState::Done) {
            plan.mark_done(&task.id);
            synchronized = true;
        }
    }
    if synchronized {
        fs::write(plan_path, plan.to_string())
            .with_context(|| format!("writing approved plan state to {plan_path:?}"))?;
        graph.index_plan(&plan_relative, &plan)?;
    }

    let task = match plan.ready_tasks().into_iter().next() {
        Some(t) => t.clone(),
        None => {
            if plan.first_undone().is_some() {
                println!("No runnable task: dependencies pending.");
            } else {
                println!("Plan is complete.");
            }
            return Ok(());
        }
    };

    let task_node_name = format!("{}#{}", plan_relative, task.id);
    let task_node_id = match graph
        .find_node_by_name(NodeKind::Task, &task_node_name)
        .with_context(|| format!("finding task node {}", task_node_name))?
    {
        Some(n) => n.id,
        None => bail!("task node {} missing after indexing plan", task_node_name),
    };

    println!("Next task: {}", task.description);

    if let Some(stop) = &task.stop {
        println!("Stop point: {}", stop);
        println!("Approve and mark this task done, then run `foundry iterate` again.");
        graph
            .record_event(&Event::StopPointReached {
                task_id: task_node_id,
                reason: stop.clone(),
            })
            .with_context(|| format!("recording stop point event for {}", task.id))?;
        return Ok(());
    }

    let run_cmd = match &task.run {
        Some(cmd) => cmd,
        None => {
            println!("Task has no run command. Skipping (mark done manually).");
            return Ok(());
        }
    };

    graph
        .record_event(&Event::TaskStarted {
            task_id: task_node_id,
            description: task.description.clone(),
        })
        .with_context(|| format!("recording task started event for {}", task.id))?;

    let interpolated = run_cmd
        .replace("{{root}}", &root.to_string_lossy())
        .replace("{{db}}", &db.to_string_lossy());

    let command = safe_job_command(&interpolated)?;
    let task_key = format!("{}#{}", plan_relative, task.id);
    drop(graph);
    println!("Running isolated attempt: {}", interpolated);
    cmd_job_run(JobRunRequest {
        root: root.to_path_buf(),
        db: db.to_path_buf(),
        task: task_key.clone(),
        idempotency_key: None,
        artifacts: Vec::new(),
        image: std::env::var("SANDBOX_IMAGE")
            .unwrap_or_else(|_| "docker.io/rust:1-bookworm".into()),
        timeout: 300,
        cpus: Some(2),
        memory: Some(2_147_483_648),
        network: false,
        command,
    })?;
    println!("Task is awaiting review; approve its job evidence before the plan can advance.");
    Ok(())
}

fn cmd_propose(
    query: Option<&str>,
    plan_path: &Path,
    root: &Path,
    db: &Path,
    model: &str,
) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    // 1. Collect the feature description.
    let description = match query {
        Some(q) => q.trim().to_string(),
        None => {
            print!("Describe the feature: ");
            std::io::stdout().flush().context("flushing stdout")?;
            let mut buf = String::new();
            std::io::stdin()
                .read_line(&mut buf)
                .context("reading feature description from stdin")?;
            buf.trim().to_string()
        }
    };
    if description.is_empty() {
        bail!("feature description is empty");
    }

    // 2. Gather relevant code context from the graph.
    let safe_query = sanitize_query(&description);
    let context_results = graph
        .search_code(&safe_query)
        .context("searching code for context")?;
    let mut context = String::new();
    for (i, (node, content)) in context_results.iter().take(5).enumerate() {
        context.push_str(&format!(
            "--- snippet {}: {} ---\n{}\n\n",
            i + 1,
            node.name,
            content.chars().take(1500).collect::<String>()
        ));
    }
    if context.is_empty() {
        context.push_str("(no indexed code context found)");
    }

    let system_prompt = "You are Foundry, a senior engineer helping turn feature ideas into concrete implementation tasks for a Rust project.";

    let mut messages: Vec<(String, String)> =
        vec![("system".to_string(), system_prompt.to_string())];

    // 3. First LLM turn: ask clarifying questions.
    messages.push((
        "user".to_string(),
        format!(
            "MODE: question-mode\n\nThe user wants to add this feature: \"{}\"\n\nRelevant codebase context:\n\n{}\n\nYour job: ask 2-4 focused clarifying questions.\nRules:\n- Ask ONLY questions.\n- Do NOT propose tasks, files, commands, or a spec.\n- Do NOT write code or implementation details.",
            description, context
        ),
    ));

    let (questions, prompt_tokens, _completion_tokens) =
        chat_with_model(model, &messages).context("asking clarifying questions")?;
    graph
        .record_event(&Event::ModelInvoked {
            model: model.to_string(),
            prompt_tokens,
            cost_usd: 0.0,
        })
        .context("recording model invocation")?;
    println!("\n{}\n", questions);

    // 4. Read the user's answers.
    println!("Your answers (end with a blank line):");
    std::io::stdout().flush().context("flushing stdout")?;
    let mut answers = String::new();
    let stdin = std::io::stdin();
    loop {
        let mut line = String::new();
        stdin
            .read_line(&mut line)
            .context("reading answers from stdin")?;
        if line.trim().is_empty() {
            break;
        }
        answers.push_str(&line);
    }
    if answers.trim().is_empty() {
        bail!("no answers provided; aborting");
    }

    // 5. Second LLM turn: produce spec and tasks.
    messages.push(("assistant".to_string(), questions));
    messages.push((
        "user".to_string(),
        format!(
            "MODE: proposal-mode\n\nThe user answered:\n\n{}\n\nYour job: produce a concise feature spec and a numbered list of concrete implementation tasks.\nRules:\n- Start with one line: Spec: <2-4 sentence summary>\n- Then add a section: ## Tasks\n- Each task must use the exact format: N. [ ] Task description - files: path1, path2 - run: command\n- Only include files you are confident about. Omit - files: if unknown.\n- Only include a - run: command if there is a clear one (e.g. `cargo test`, `just check`). Omit if not.\n- Keep tasks small and concrete. Prefer 3-7 tasks.\n- Do NOT ask questions.\n- Do NOT include meta-tasks like \"discuss with user\" or \"verify with user\".",
            answers
        ),
    ));

    let (mut proposal, prompt_tokens2, _completion_tokens2) =
        chat_with_model(model, &messages).context("generating feature proposal")?;
    graph
        .record_event(&Event::ModelInvoked {
            model: model.to_string(),
            prompt_tokens: prompt_tokens2,
            cost_usd: 0.0,
        })
        .context("recording model invocation")?;

    // 6. Confirm, edit, or abort.
    loop {
        let (spec, tasks) = parse_proposal(&proposal);
        if tasks.is_empty() {
            println!(
                "\nCould not parse any tasks from the model response. Raw output:\n{}\n",
                proposal
            );
            print!("[r=retry / n=abort]: ");
            std::io::stdout().flush()?;
            let mut choice = String::new();
            std::io::stdin().read_line(&mut choice)?;
            if choice.trim().eq_ignore_ascii_case("r") {
                messages.push(("assistant".to_string(), proposal));
                messages.push((
                    "user".to_string(),
                    "MODE: proposal-mode\n\nYour previous response did not contain any parseable tasks. Please try again.\nRules:\n- Start with one line: Spec: <summary>\n- Then add a section: ## Tasks\n- Each task must use the exact format: N. [ ] Task description - files: path1, path2 - run: command\n- Do NOT ask questions.".to_string(),
                ));
                let (new_proposal, pt, _ct) =
                    chat_with_model(model, &messages).context("retrying feature proposal")?;
                graph.record_event(&Event::ModelInvoked {
                    model: model.to_string(),
                    prompt_tokens: pt,
                    cost_usd: 0.0,
                })?;
                proposal = new_proposal;
                continue;
            } else {
                println!("Aborted. No tasks added.");
                return Ok(());
            }
        }

        println!("\n=== Proposed Feature ===\n{}\n", spec);
        println!("=== Tasks to add to {} ===", plan_path.display());
        for (i, (desc, files, run)) in tasks.iter().enumerate() {
            let mut line = format!("{}. [ ] {}", i + 1, desc);
            if !files.is_empty() {
                line.push_str(&format!(" - files: {}", files.join(", ")));
            }
            if let Some(run_cmd) = run {
                line.push_str(&format!(" - run: {}", run_cmd));
            }
            println!("{}", line);
        }

        print!("\nApprove? [y=append to plan / e=edit / n=abort]: ");
        std::io::stdout().flush()?;
        let mut choice = String::new();
        std::io::stdin().read_line(&mut choice)?;
        match choice.trim().to_lowercase().as_str() {
            "y" | "yes" => {
                if let Some(parent) = plan_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("creating plan directory {:?}", parent))?;
                }
                let plan_text = fs::read_to_string(plan_path)
                    .unwrap_or_else(|_| "# Feature Backlog\n\n".to_string());
                let mut plan = Plan::parse("features", &plan_text);
                for (desc, files, run) in &tasks {
                    plan.append_task(desc, files, run.as_deref());
                }
                fs::write(plan_path, plan.to_string())
                    .with_context(|| format!("writing plan to {:?}", plan_path))?;

                let appended_count = tasks.len();
                let task_ids: Vec<String> = plan
                    .tasks
                    .iter()
                    .rev()
                    .take(appended_count)
                    .rev()
                    .map(|t| t.id.clone())
                    .collect();
                let plan_relative = plan_path
                    .strip_prefix(root)
                    .unwrap_or(plan_path)
                    .to_string_lossy()
                    .to_string();
                graph
                    .record_event(&Event::FeatureProposed {
                        title: description.clone(),
                        plan_path: plan_relative,
                        task_ids,
                    })
                    .context("recording feature proposed event")?;
                println!(
                    "Added {} task(s) to {}.",
                    appended_count,
                    plan_path.display()
                );
                return Ok(());
            }
            "e" | "edit" => {
                println!("What should change? (end with a blank line):");
                std::io::stdout().flush()?;
                let mut feedback = String::new();
                loop {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    if line.trim().is_empty() {
                        break;
                    }
                    feedback.push_str(&line);
                }
                messages.push(("assistant".to_string(), proposal));
                messages.push((
                    "user".to_string(),
                    format!(
                        "Please revise the proposal based on this feedback:\n\n{}",
                        feedback
                    ),
                ));
                let (new_proposal, pt, _ct) =
                    chat_with_model(model, &messages).context("revising feature proposal")?;
                graph.record_event(&Event::ModelInvoked {
                    model: model.to_string(),
                    prompt_tokens: pt,
                    cost_usd: 0.0,
                })?;
                proposal = new_proposal;
                continue;
            }
            _ => {
                println!("Aborted. No tasks added.");
                return Ok(());
            }
        }
    }
}

fn cmd_snapshot(action: SnapshotAction) -> Result<()> {
    match action {
        SnapshotAction::Create { root, db, name } => cmd_snapshot_create(&root, &db, name),
        SnapshotAction::List { root, db } => cmd_snapshot_list(&root, &db),
        SnapshotAction::Restore {
            root,
            db,
            name,
            force,
        } => cmd_snapshot_restore(&root, &db, &name, force),
    }
}

fn snapshot_dir(db: &Path) -> PathBuf {
    db.parent()
        .map(|p| p.join("snapshots"))
        .unwrap_or_else(|| PathBuf::from("snapshots"))
}

fn cmd_snapshot_create(_root: &Path, db: &Path, name: Option<String>) -> Result<()> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    drop(graph);

    let name = name.unwrap_or_else(|| chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
    let snapshot_name = format!("{}.sqlite", name);
    let dir = snapshot_dir(db);
    fs::create_dir_all(&dir).with_context(|| format!("creating snapshot directory {:?}", dir))?;
    let dest = dir.join(&snapshot_name);

    fs::copy(db, &dest).with_context(|| format!("copying {:?} to {:?}", db, dest))?;

    let mut graph = Graph::open(db).with_context(|| format!("reopening graph at {:?}", db))?;
    graph.record_event(&Event::SnapshotCreated {
        name: name.clone(),
        path: dest.to_string_lossy().to_string(),
    })?;

    println!("Created snapshot: {} -> {:?}", name, dest);
    Ok(())
}

fn cmd_snapshot_list(_root: &Path, db: &Path) -> Result<()> {
    let dir = snapshot_dir(db);
    if !dir.exists() {
        println!("No snapshots found.");
        return Ok(());
    }

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .with_context(|| format!("reading snapshot directory {:?}", dir))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "sqlite")
                .unwrap_or(false)
        })
        .collect();

    if entries.is_empty() {
        println!("No snapshots found.");
        return Ok(());
    }

    entries.sort_by_key(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });

    for entry in entries {
        println!("{}", entry.file_name().to_string_lossy());
    }
    Ok(())
}

fn cmd_snapshot_restore(_root: &Path, db: &Path, name: &str, force: bool) -> Result<()> {
    let dir = snapshot_dir(db);
    let src = dir.join(format!("{}.sqlite", name));
    if !src.exists() {
        bail!("snapshot not found: {:?}", src);
    }

    if !force {
        print!(
            "This will overwrite {:?} with {:?}. Proceed? [y/N]: ",
            db, src
        );
        std::io::stdout().flush()?;
        let mut choice = String::new();
        std::io::stdin().read_line(&mut choice)?;
        if !matches!(choice.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Restore cancelled.");
            return Ok(());
        }
    }

    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    drop(graph);

    fs::copy(&src, db).with_context(|| format!("copying {:?} to {:?}", src, db))?;

    let mut graph = Graph::open(db).with_context(|| format!("reopening graph at {:?}", db))?;
    graph.record_event(&Event::SnapshotRestored {
        name: name.to_string(),
        path: src.to_string_lossy().to_string(),
    })?;

    println!("Restored snapshot {} to {:?}", name, db);
    Ok(())
}

fn chat_with_model(model: &str, messages: &[(String, String)]) -> Result<(String, u64, u64)> {
    let chat_messages: Vec<ChatMessage<'_>> = messages
        .iter()
        .map(|(role, content)| ChatMessage {
            role: role.as_str(),
            content: content.as_str(),
        })
        .collect();
    ask_ollama(model, &chat_messages)
}

type ProposedTask = (String, Vec<String>, Option<String>);

fn parse_proposal(text: &str) -> (String, Vec<ProposedTask>) {
    let mut spec = String::new();
    let mut tasks = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.to_lowercase().starts_with("spec:") {
            spec = trimmed
                .strip_prefix("spec:")
                .or_else(|| trimmed.strip_prefix("Spec:"))
                .unwrap_or("")
                .trim()
                .to_string();
        } else if trimmed.starts_with(|c: char| c.is_ascii_digit())
            && trimmed.contains("[ ]")
            && let Some(task) = parse_task_line(trimmed)
        {
            tasks.push(task);
        }
    }
    (spec, tasks)
}

fn parse_task_line(line: &str) -> Option<ProposedTask> {
    let marker = if line.contains(". [ ] ") {
        ". [ ] "
    } else if line.contains("- [ ] ") {
        "- [ ] "
    } else {
        return None;
    };
    let idx = line.find(marker)?;
    let rest = &line[idx + marker.len()..];

    let mut description = rest.to_string();
    let mut files = Vec::new();
    let mut run = None;

    if let Some(files_start) = rest.find(" - files:") {
        description = rest[..files_start].trim().to_string();
        let files_part = &rest[files_start + " - files:".len()..];
        let files_end = files_part.find(" - run:").unwrap_or(files_part.len());
        files = files_part[..files_end]
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(run_start) = rest.find(" - run:") {
            run = Some(rest[run_start + " - run:".len()..].trim().to_string());
        }
    } else if let Some(run_start) = rest.find(" - run:") {
        description = rest[..run_start].trim().to_string();
        run = Some(rest[run_start + " - run:".len()..].trim().to_string());
    }

    if description.is_empty() {
        return None;
    }

    Some((description, files, run))
}

fn safe_job_command(cmd: &str) -> Result<Vec<String>> {
    if cmd.chars().any(|c| ";|&<>$`\n\"'".contains(c)) {
        bail!("refusing command with shell metacharacters: {}", cmd);
    }
    if !is_whitelisted(cmd) {
        bail!("command not in safe whitelist: {}", cmd);
    }

    Ok(cmd.split_whitespace().map(str::to_owned).collect())
}

fn is_whitelisted(cmd: &str) -> bool {
    let prefixes = [
        "cargo build",
        "cargo test",
        "cargo fmt",
        "cargo clippy",
        "cargo check",
        "just build",
        "just check",
        "just deploy",
        "just test",
        "just init",
        "just index",
        "just plan",
        "just rebuild",
        "just reconcile",
        "just sandbox",
        "foundry index",
        "foundry rebuild",
        "foundry reconcile",
    ];
    prefixes
        .iter()
        .any(|p| cmd == *p || cmd.starts_with(&format!("{} ", p)))
}

fn is_ignored(path: &Path) -> bool {
    let components: Vec<_> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    components.iter().any(|c| {
        matches!(
            c.as_str(),
            ".git" | "target" | "node_modules" | ".foundry" | ".cache" | "dist" | "build"
        )
    })
}
