use anyhow::Result;
use clap::{Parser, Subcommand};
use foundry_core::{JobId, ReviewDecision};
use std::path::{Path, PathBuf};

mod agent_sandbox;
mod attempt;
mod cgroup_policy;
mod commands;
mod digest_boundary;
mod indexer;
mod lease;
mod manifest;
mod promotion;
mod review_policy;
mod review_session;
mod runner;
mod sweep;
mod task_contract;
mod tdd_policy;
mod telemetry;

const SOCRATIC_DISCOURSE_CONTRACT: &str = "SOCRATIC DISCOURSE CONTRACT
- Begin from one shared, decision-bearing question.
- Distinguish observed evidence from assumptions.
- Surface at least one plausible competing interpretation.
- Ask what evidence would falsify the current interpretation.
- Ask only questions that can change a decision or next action.
- Synthesize concisely; do not use questions to evade a justified conclusion.
- The model is a partner in inquiry. The human remains the accountable decision-maker.";

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
        #[arg(long, default_value = "docker.io/rust:1.92-bookworm")]
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
        /// Emit the complete durable job result as JSON instead of a human summary.
        #[arg(long)]
        json: bool,
        /// Give this job's evidence a DeleteAfter retention of N days instead
        /// of the default 30-day ReviewAfter policy.
        #[arg(long, env = "FOUNDRY_EVIDENCE_RETENTION_DAYS")]
        evidence_retention_days: Option<i64>,
        /// Allow the sandbox to silently drop CPU/memory limits if the
        /// container runtime reports it cannot enforce them. The default is
        /// to fail closed so that a requested security boundary is never
        /// degraded without explicit operator consent.
        #[arg(long, env = "FOUNDRY_ALLOW_CGROUP_FALLBACK")]
        allow_cgroup_fallback: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Report governed evidence dispositions; --enforce erases delete-due
    /// evidence through the lethe erasure contract and records receipts.
    Sweep {
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Erase delete-due evidence; without this flag the sweep is a dry-run report.
        #[arg(long)]
        enforce: bool,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show whether an iteration currently holds the repository lease.
    Lease {
        /// Project root containing .foundry/.
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Approve successful job evidence and complete its task.
    ReviewApprove {
        #[arg(long, default_value = ".")]
        root: PathBuf,
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
        #[arg(long, default_value = ".")]
        root: PathBuf,
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
    /// Compare two generated review drafts and make the authoritative human decision.
    ReviewTui {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        #[arg(long)]
        task: String,
        #[arg(long)]
        job: uuid::Uuid,
        #[arg(long)]
        reviewer: String,
        /// Review agent command; falls back to FOUNDRY_REVIEW_AGENT_COMMAND then FOUNDRY_AGENT_COMMAND.
        #[arg(long)]
        agent_command: Option<String>,
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
    /// Audit plan/graph identity in both directions: legacy positional
    /// task keys, orphaned graph state, derived ids, and done-state drift.
    /// --apply performs the safe repairs and records the event.
    ReconcilePlan {
        /// Path to the plan file.
        #[arg(long, default_value = "./plans/bootstrap.plan.md")]
        plan: PathBuf,
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Migrate legacy keys, persist explicit id tags into the plan
        /// file, and reindex. Orphaned keys are only ever reported.
        #[arg(long)]
        apply: bool,
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
        #[arg(long, default_value = "./plans/features.plan.md")]
        plan: PathBuf,
        /// Project root (for commands that need it).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Use a red-green TDD loop driven by an external editor agent.
        #[arg(long)]
        tdd: bool,
        /// Editor agent command. The prompt is provided on stdin.
        #[arg(long, env = "FOUNDRY_AGENT_COMMAND")]
        agent_command: Option<String>,
        /// Print the effective sandbox configuration and run a toolchain preflight.
        #[arg(long)]
        debug_runner: bool,
        /// Refuse to run an acceptance check that has never been observed
        /// failing (plain mode always is); use --tdd to satisfy this.
        #[arg(long)]
        require_falsified: bool,
        /// Allow the sandbox to silently drop CPU/memory limits if the
        /// container runtime reports it cannot enforce them. The default is
        /// to fail closed.
        #[arg(long, env = "FOUNDRY_ALLOW_CGROUP_FALLBACK")]
        allow_cgroup_fallback: bool,
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
        action: commands::snapshot::SnapshotAction,
    },
}

fn main() -> Result<()> {
    telemetry::init()?;
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
            json,
            evidence_retention_days,
            allow_cgroup_fallback,
            command,
        } => commands::job::cmd_job_run(commands::job::JobRunRequest {
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
            json,
            workspace_baseline: None,
            staged: false,
            evidence_retention_days,
            acceptance_authority: None,
            allow_cgroup_fallback,
            command,
        }),
        Commands::Sweep { db, enforce, json } => {
            // An enforcing sweep deletes evidence; hold the repository lease so
            // it cannot race a concurrent iteration's job runs (sweep.rs D7).
            let _lease = if enforce {
                let foundry_dir = db
                    .parent()
                    .filter(|dir| !dir.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                Some(
                    lease::acquire(&foundry_dir, &lease::default_owner(), "sweep --enforce")
                        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?,
                )
            } else {
                None
            };
            sweep::run_sweep(&db, enforce, json)
        }
        Commands::Lease { root } => commands::plan::cmd_lease(&root),
        Commands::ReviewApprove {
            root,
            db,
            task,
            job,
            reviewer,
            reason,
        } => commands::review::cmd_review(
            &root,
            &db,
            &task,
            JobId(job),
            ReviewDecision::Approve,
            &reviewer,
            &reason,
        ),
        Commands::ReviewReject {
            root,
            db,
            task,
            job,
            reviewer,
            reason,
        } => commands::review::cmd_review(
            &root,
            &db,
            &task,
            JobId(job),
            ReviewDecision::Reject,
            &reviewer,
            &reason,
        ),
        Commands::ReviewTui {
            root,
            db,
            task,
            job,
            reviewer,
            agent_command,
        } => commands::review::cmd_review_tui(
            &root,
            &db,
            &task,
            JobId(job),
            &reviewer,
            agent_command.as_deref(),
        ),
        Commands::Init { root } => commands::plan::cmd_init(&root),
        Commands::Index { root, db, embed } => indexer::index(&root, &db, embed),
        Commands::Plan => commands::plan::cmd_plan(),
        Commands::List { db, kind } => commands::plan::cmd_list(&db, kind.as_deref()),
        Commands::Search { db, query } => commands::plan::cmd_search(&db, &query),
        Commands::SemSearch {
            db,
            model,
            limit,
            query,
        } => commands::ask::cmd_semsearch(&db, &model, &query, limit),
        Commands::Rebuild { root, db, embed } => indexer::rebuild(&root, &db, embed),
        Commands::Reconcile { root, db } => commands::plan::cmd_reconcile(&root, &db),
        Commands::ReconcilePlan {
            plan,
            root,
            db,
            apply,
        } => commands::plan::cmd_reconcile_plan(&plan, &root, &db, apply),
        Commands::Heal { root, db } => commands::plan::cmd_heal(&root, &db),
        Commands::Doctor { root, db, plan } => commands::plan::cmd_doctor(&root, &db, &plan),
        Commands::CheckRules { db } => commands::plan::cmd_check_rules(&db),
        Commands::ApproveRule { db, rule_id } => commands::plan::cmd_approve_rule(&db, &rule_id),
        Commands::Ask {
            db,
            model,
            limit,
            query,
        } => commands::ask::cmd_ask(&db, &model, &query, limit),
        Commands::Iterate {
            plan,
            root,
            db,
            tdd,
            agent_command,
            debug_runner,
            require_falsified,
            allow_cgroup_fallback,
        } => commands::iterate::cmd_iterate(
            &plan,
            &root,
            &db,
            commands::iterate::IterateOptions {
                tdd,
                agent_command: agent_command.as_deref(),
                debug_runner,
                require_falsified,
                allow_cgroup_fallback,
            },
        ),
        Commands::Propose {
            query,
            plan,
            root,
            db,
            model,
        } => commands::propose::cmd_propose(query.as_deref(), &plan, &root, &db, &model),
        Commands::Snapshot { action } => commands::snapshot::cmd_snapshot(action),
    }
}

#[cfg(test)]
mod tests {
    use super::SOCRATIC_DISCOURSE_CONTRACT;
    use crate::commands::iterate::{IterationFeedback, feedback_prompt};
    use crate::commands::job::{CargoTestSummary, cargo_test_summary};

    #[test]
    fn rejected_review_is_rendered_as_durable_agent_feedback() {
        let feedback = IterationFeedback {
            text: "Reason: never overwrite an existing checksum".into(),
            infrastructure_only: false,
        };
        let prompt = feedback_prompt(Some(&feedback));
        assert!(prompt.contains("rejected review as an acceptance constraint"));
        assert!(prompt.contains("never overwrite an existing checksum"));
        assert!(prompt.contains("untrusted diagnostic data"));
    }

    #[test]
    fn cargo_test_output_is_collapsed_into_a_workspace_summary() {
        let output = "\
running 8 tests\n\
test result: ok. 8 passed; 0 failed; 1 ignored; finished in 0.01s\n\n\
running 46 tests\n\
test result: ok. 46 passed; 0 failed; 0 ignored; finished in 0.04s\n\n\
running 0 tests\n\
test result: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s\n";

        assert_eq!(
            cargo_test_summary(output),
            Some(CargoTestSummary {
                suites: 3,
                passed: 54,
                failed: 0,
                ignored: 1,
            })
        );
    }

    #[test]
    fn model_interactions_share_one_socratic_contract() {
        for principle in [
            "shared, decision-bearing question",
            "observed evidence from assumptions",
            "competing interpretation",
            "falsify",
            "human remains the accountable decision-maker",
        ] {
            assert!(SOCRATIC_DISCOURSE_CONTRACT.contains(principle));
        }
    }
}
