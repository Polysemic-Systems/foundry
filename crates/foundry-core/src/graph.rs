use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use thiserror::Error;
use uuid::Uuid;

use crate::Event;
use crate::job::{
    Job, JobId, JobResult, JobState, Review, ReviewDecision, StateParseError, TaskState,
    TransitionError,
};
use crate::living::ConformanceError;
use crate::{
    DiscourseAct, DiscourseSpeaker, DiscourseTurn, ReviewDraft, ReviewPerspective, ReviewResolution,
};

pub const LATEST_SCHEMA_VERSION: i64 = 7;

/// One raw `job_results` row, deserialized but never hydrated: hydration
/// fails closed on missing blobs, and enumeration must survive corrupt rows.
#[derive(Debug)]
pub struct JobResultRow {
    /// Raw column text; may not be a valid uuid in corrupt history.
    pub job_id: String,
    /// Stable task key from the owning job, when that join still resolves.
    pub task_key: Option<String>,
    pub created_at: String,
    /// The stored JSON exactly as persisted.
    pub raw: String,
    /// Deserialization outcome; an error is data for quarantine, not a crash.
    pub parsed: Result<JobResult, String>,
}

/// Collect `sha256:<64 hex>` tokens from raw text into `digests`.
fn scan_sha256_tokens(raw: &str, digests: &mut BTreeSet<String>) {
    let mut remainder = raw;
    while let Some(position) = remainder.find("sha256:") {
        let candidate = &remainder[position + "sha256:".len()..];
        let hex: String = candidate
            .chars()
            .take_while(|character| character.is_ascii_hexdigit())
            .take(64)
            .collect();
        if hex.len() == 64 {
            digests.insert(format!("sha256:{hex}"));
        }
        remainder = &remainder[position + "sha256:".len()..];
    }
}

/// A unique node identifier in the production graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub Uuid);

impl NodeId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The kind of thing a node represents.
/// Each kind owns its own domain language and payload schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Work item: intent, task, bug, spike.
    Task,
    /// Source code, config, schema.
    Code,
    /// Test, scenario, benchmark.
    Test,
    /// Review, approval, gate.
    Review,
    /// Deployment, rollout, release.
    Deploy,
    /// Customer feedback, crash, ticket.
    Feedback,
    /// Harness rule, lint, policy.
    Rule,
    /// Model, capability, agent configuration.
    Model,
    /// Runtime environment, sandbox, runner.
    Env,
    /// A plan: markdown-plan DAG.
    Plan,
}

impl NodeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::Task => "task",
            NodeKind::Code => "code",
            NodeKind::Test => "test",
            NodeKind::Review => "review",
            NodeKind::Deploy => "deploy",
            NodeKind::Feedback => "feedback",
            NodeKind::Rule => "rule",
            NodeKind::Model => "model",
            NodeKind::Env => "env",
            NodeKind::Plan => "plan",
        }
    }
}

impl std::str::FromStr for NodeKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "task" => Ok(NodeKind::Task),
            "code" => Ok(NodeKind::Code),
            "test" => Ok(NodeKind::Test),
            "review" => Ok(NodeKind::Review),
            "deploy" => Ok(NodeKind::Deploy),
            "feedback" => Ok(NodeKind::Feedback),
            "rule" => Ok(NodeKind::Rule),
            "model" => Ok(NodeKind::Model),
            "env" => Ok(NodeKind::Env),
            "plan" => Ok(NodeKind::Plan),
            _ => Err(format!("unknown node kind: {}", s)),
        }
    }
}

/// A node in the production graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

impl Node {
    pub fn new(kind: NodeKind, name: impl Into<String>, payload: Value) -> Self {
        Self {
            id: NodeId::new(),
            kind,
            name: name.into(),
            payload,
            created_at: Utc::now(),
        }
    }
}

/// Relationship between two nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A depends on B.
    DependsOn,
    /// A implements B.
    Implements,
    /// A tests B.
    Tests,
    /// A reviews B.
    Reviews,
    /// A deploys B.
    Deploys,
    /// A learns from B.
    LearnsFrom,
    /// A contains B.
    Contains,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::DependsOn => "depends_on",
            EdgeKind::Implements => "implements",
            EdgeKind::Tests => "tests",
            EdgeKind::Reviews => "reviews",
            EdgeKind::Deploys => "deploys",
            EdgeKind::LearnsFrom => "learns_from",
            EdgeKind::Contains => "contains",
        }
    }
}

impl std::str::FromStr for EdgeKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "depends_on" => Ok(EdgeKind::DependsOn),
            "implements" => Ok(EdgeKind::Implements),
            "tests" => Ok(EdgeKind::Tests),
            "reviews" => Ok(EdgeKind::Reviews),
            "deploys" => Ok(EdgeKind::Deploys),
            "learns_from" => Ok(EdgeKind::LearnsFrom),
            "contains" => Ok(EdgeKind::Contains),
            _ => Err(format!("unknown edge kind: {}", s)),
        }
    }
}

/// An edge in the production graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: Uuid,
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("node not found: {0}")]
    NodeNotFound(NodeId),
    #[error(transparent)]
    InvalidTransition(#[from] TransitionError),
    #[error("job result does not match persisted job state")]
    ResultStateMismatch,
    #[error("invalid stored job result: {0}")]
    InvalidStoredResult(#[from] serde_json::Error),
    #[error(transparent)]
    NonconformingEvidence(#[from] ConformanceError),
    #[error(transparent)]
    EvidenceStore(#[from] crate::evidence_store::EvidenceStoreError),
    #[error("invalid discourse turn: {0}")]
    InvalidDiscourse(String),
    #[error("checksum for migration {version} does not match known migration SQL")]
    ChecksumMismatch { version: i64 },
    #[error("unknown migration version {version} with no known migration content")]
    UnknownMigration { version: i64 },
}

#[derive(Debug)]
struct ColumnParseError(String);

impl std::fmt::Display for ColumnParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ColumnParseError {}

fn state_sql_error(column: usize, error: StateParseError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

fn parse_task_state(value: String) -> Result<TaskState, rusqlite::Error> {
    value.parse().map_err(|error| state_sql_error(0, error))
}

fn parse_job_state_value(value: &str) -> Result<JobState, GraphError> {
    value
        .parse()
        .map_err(|error| GraphError::Sqlite(state_sql_error(3, error)))
}

fn map_job(row: &rusqlite::Row<'_>) -> Result<Job, rusqlite::Error> {
    let id: String = row.get(0)?;
    let id = Uuid::parse_str(&id).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let state: String = row.get(3)?;
    let state = state.parse().map_err(|error| state_sql_error(3, error))?;
    Ok(Job {
        id: JobId(id),
        task_key: row.get(1)?,
        idempotency_key: row.get(2)?,
        state,
    })
}

/// The production graph: nodes, edges, and persistence.
pub struct Graph {
    conn: Connection,
}

impl Graph {
    pub fn open(path: &Path) -> Result<Self, GraphError> {
        let conn = Connection::open(path)?;
        let mut graph = Self { conn };
        graph.migrate()?;
        Ok(graph)
    }

    pub fn open_in_memory() -> Result<Self, GraphError> {
        let conn = Connection::open_in_memory()?;
        let mut graph = Self { conn };
        graph.migrate()?;
        Ok(graph)
    }

    fn migrate(&mut self) -> Result<(), GraphError> {
        // SQLite disables foreign-key enforcement for each new connection.
        self.conn.pragma_update(None, "foreign_keys", "ON")?;

        // WAL mode: readers do not block writers and writes are durable.
        self.conn.pragma_update(None, "journal_mode", "WAL")?;

        // Ensure the schema_migrations table matches the shared definition so
        // that Graph and the exported MigrationStorage API operate on the same
        // table shape (version, checksum, applied_at).
        crate::db::schema::ensure_schema_migrations_table(&self.conn)?;

        let current: i64 = self
            .conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);

        let migrations = crate::migration_registry::migrations();

        // Look up the expected SHA-256 checksum for every known migration from the
        // canonical registry so that legacy rows are backfilled with the same
        // digest that other consumers (such as MigrationStorage) would compute.
        let expected_checksums: std::collections::HashMap<i64, String> = migrations
            .iter()
            .map(|(version, _)| {
                (
                    *version,
                    crate::migration_registry::checksum_for(*version)
                        .expect("migration has an expected checksum"),
                )
            })
            .collect();

        // Validate and backfill every row already in schema_migrations before
        // applying any new migrations. Unknown versions or mismatched checksums
        // are treated as data integrity errors.
        let mut stmt = self
            .conn
            .prepare("SELECT version, checksum FROM schema_migrations")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (version, checksum) = row?;
            let expected = expected_checksums
                .get(&version)
                .ok_or(GraphError::UnknownMigration { version })?;
            let stored = checksum.unwrap_or_default();
            if stored.is_empty() {
                self.conn.execute(
                    "UPDATE schema_migrations SET checksum = ?1 WHERE version = ?2",
                    params![expected, version],
                )?;
            } else if stored != *expected {
                return Err(GraphError::ChecksumMismatch { version });
            }
        }

        for (version, sql) in migrations {
            if version > current {
                self.conn.execute_batch(sql)?;
                let checksum = expected_checksums
                    .get(&version)
                    .expect("migration has an expected checksum")
                    .clone();
                self.conn.execute(
                    "INSERT INTO schema_migrations (version, checksum, applied_at)
                     VALUES (?1, ?2, ?3)",
                    params![version, checksum, Utc::now().to_rfc3339()],
                )?;
            }
        }

        Ok(())
    }

    /// Persist an event to the graph's event log.
    pub fn record_event(&mut self, event: &Event) -> Result<(), GraphError> {
        self.emit_event(event)
    }

    fn emit_event(&mut self, event: &Event) -> Result<(), GraphError> {
        let payload = serde_json::to_string(event).expect("event serializes");
        self.conn.execute(
            "INSERT INTO events (id, kind, payload, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                Uuid::new_v4().to_string(),
                event.kind(),
                payload,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Drop indexed/derived state. Events, migrations, durable lifecycle state,
    /// and rule approvals remain.
    /// After this, the caller should re-index the codebase.
    pub fn truncate_derived(&mut self) -> Result<(), GraphError> {
        self.conn.execute("DELETE FROM code_search", [])?;
        self.conn.execute("DELETE FROM code_index", [])?;
        self.conn.execute("DELETE FROM code_embeddings", [])?;
        self.conn.execute("DELETE FROM edges", [])?;
        self.conn
            .execute("DELETE FROM nodes WHERE kind != 'rule'", [])?;
        Ok(())
    }

    pub fn wal_mode(&self) -> Result<bool, GraphError> {
        let mode: String = self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        Ok(mode.eq_ignore_ascii_case("wal"))
    }

    pub fn schema_version(&self) -> Result<i64, GraphError> {
        let version: i64 = self
            .conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);
        Ok(version)
    }

    pub fn events(&self, limit: usize) -> Result<Vec<(DateTime<Utc>, Event)>, GraphError> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload, created_at FROM events ORDER BY created_at DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let payload: String = row.get(0)?;
            let ts: String = row.get(1)?;
            let event: Event = serde_json::from_str(&payload).expect("stored event is valid json");
            let created_at = DateTime::parse_from_rfc3339(&ts)
                .expect("valid timestamp")
                .with_timezone(&Utc);
            Ok((created_at, event))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(GraphError::Sqlite)
    }

    /// Append an immutable, reply-linked turn to a Socratic discourse.
    pub fn record_discourse_turn(&mut self, turn: &DiscourseTurn) -> Result<(), GraphError> {
        if turn.context_key.trim().is_empty() || turn.body.trim().is_empty() {
            return Err(GraphError::InvalidDiscourse(
                "context and body must be non-empty".into(),
            ));
        }
        if let Some(reply_to) = turn.reply_to {
            let parent_context: Option<String> = self
                .conn
                .query_row(
                    "SELECT context_key FROM discourse_turns WHERE id = ?1",
                    params![reply_to.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if parent_context.as_deref() != Some(turn.context_key.as_str()) {
                return Err(GraphError::InvalidDiscourse(
                    "a reply must reference a turn in the same context".into(),
                ));
            }
        }
        self.conn.execute(
            "INSERT INTO discourse_turns
             (id, context_key, speaker, act, body, reply_to, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                turn.id.to_string(),
                turn.context_key,
                turn.speaker.as_str(),
                turn.act.as_str(),
                turn.body,
                turn.reply_to.map(|id| id.to_string()),
                turn.created_at.to_rfc3339(),
            ],
        )?;
        self.emit_event(&Event::DiscourseTurnRecorded {
            turn_id: turn.id,
            context_key: turn.context_key.clone(),
            act: turn.act.as_str().into(),
        })
    }

    /// Read a discourse in chronological order for later learning or review.
    pub fn discourse_for_context(
        &self,
        context_key: &str,
    ) -> Result<Vec<DiscourseTurn>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, speaker, act, body, reply_to, created_at
             FROM discourse_turns WHERE context_key = ?1 ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![context_key], |row| {
            let id: String = row.get(0)?;
            let speaker: String = row.get(1)?;
            let act: String = row.get(2)?;
            let reply_to: Option<String> = row.get(4)?;
            let created_at: String = row.get(5)?;
            let conversion_error = |column, value: String| {
                rusqlite::Error::FromSqlConversionFailure(
                    column,
                    rusqlite::types::Type::Text,
                    Box::new(ColumnParseError(value)),
                )
            };
            Ok(DiscourseTurn {
                id: Uuid::parse_str(&id).map_err(|_| conversion_error(0, id))?,
                context_key: context_key.into(),
                speaker: DiscourseSpeaker::from_stored(&speaker)
                    .ok_or_else(|| conversion_error(1, speaker))?,
                act: DiscourseAct::from_stored(&act).ok_or_else(|| conversion_error(2, act))?,
                body: row.get(3)?,
                reply_to: reply_to
                    .map(|value| Uuid::parse_str(&value).map_err(|_| conversion_error(4, value)))
                    .transpose()?,
                created_at: DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|_| conversion_error(5, created_at))?
                    .with_timezone(&Utc),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(GraphError::Sqlite)
    }

    /// Ensure a discourse begins with one explicit shared question.
    pub fn ensure_discourse_question(
        &mut self,
        context_key: &str,
        question: &str,
    ) -> Result<Uuid, GraphError> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM discourse_turns
                 WHERE context_key = ?1 AND act = 'question'
                 ORDER BY rowid LIMIT 1",
                params![context_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            return Uuid::parse_str(&existing).map_err(|_| {
                GraphError::InvalidDiscourse("stored question id is not a UUID".into())
            });
        }
        let turn = DiscourseTurn::new(
            context_key,
            DiscourseSpeaker::System,
            DiscourseAct::Question,
            question,
            None,
        );
        let id = turn.id;
        self.record_discourse_turn(&turn)?;
        Ok(id)
    }

    /// Initialize a durable task lifecycle. Repeating the operation is idempotent.
    pub fn initialize_task_state(
        &mut self,
        task_key: &str,
        state: TaskState,
    ) -> Result<TaskState, GraphError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO task_states (task_key, state, updated_at)
             VALUES (?1, ?2, ?3)",
            params![task_key, state.as_str(), Utc::now().to_rfc3339()],
        )?;
        self.task_state(task_key)?
            .ok_or_else(|| GraphError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
    }

    pub fn task_state(&self, task_key: &str) -> Result<Option<TaskState>, GraphError> {
        self.conn
            .query_row(
                "SELECT state FROM task_states WHERE task_key = ?1",
                params![task_key],
                |row| parse_task_state(row.get::<_, String>(0)?),
            )
            .optional()
            .map_err(GraphError::Sqlite)
    }

    pub fn transition_task(
        &mut self,
        task_key: &str,
        next: TaskState,
    ) -> Result<TaskState, GraphError> {
        let current = self
            .task_state(task_key)?
            .ok_or_else(|| GraphError::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
        let next = current.transition(next)?;
        self.conn.execute(
            "UPDATE task_states SET state = ?1, updated_at = ?2 WHERE task_key = ?3",
            params![next.as_str(), Utc::now().to_rfc3339(), task_key],
        )?;
        Ok(next)
    }

    /// Create one execution attempt, returning the original job on a repeated key.
    pub fn create_job(&mut self, task_key: &str, idempotency_key: &str) -> Result<Job, GraphError> {
        let id = JobId::new();
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR IGNORE INTO jobs
             (id, task_key, idempotency_key, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                id.0.to_string(),
                task_key,
                idempotency_key,
                JobState::Queued.as_str(),
                now
            ],
        )?;
        self.job_by_idempotency_key(idempotency_key)?
            .ok_or_else(|| GraphError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
    }

    pub fn job_by_idempotency_key(&self, key: &str) -> Result<Option<Job>, GraphError> {
        self.conn
            .query_row(
                "SELECT id, task_key, idempotency_key, state
                 FROM jobs WHERE idempotency_key = ?1",
                params![key],
                map_job,
            )
            .optional()
            .map_err(GraphError::Sqlite)
    }

    pub fn transition_job(&mut self, id: JobId, next: JobState) -> Result<JobState, GraphError> {
        let current: String = self.conn.query_row(
            "SELECT state FROM jobs WHERE id = ?1",
            params![id.0.to_string()],
            |row| row.get(0),
        )?;
        let current = parse_job_state_value(&current)?;
        let next = current.transition(next)?;
        self.conn.execute(
            "UPDATE jobs SET state = ?1, updated_at = ?2 WHERE id = ?3",
            params![next.as_str(), Utc::now().to_rfc3339(), id.0.to_string()],
        )?;
        Ok(next)
    }

    /// Store immutable, governed evidence for a terminal job. Repeating the
    /// same write is idempotent; the original evidence is never overwritten.
    pub fn record_job_result(&mut self, result: &JobResult) -> Result<(), GraphError> {
        result.governance.validate()?;
        let persisted: String = self.conn.query_row(
            "SELECT state FROM jobs WHERE id = ?1",
            params![result.job_id.0.to_string()],
            |row| row.get(0),
        )?;
        if parse_job_state_value(&persisted)? != result.state || !result.state.is_terminal() {
            return Err(GraphError::ResultStateMismatch);
        }
        let stored = crate::evidence_store::externalize_job_result(&self.conn, result)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO job_results (job_id, result, created_at) VALUES (?1, ?2, ?3)",
            params![
                result.job_id.0.to_string(),
                serde_json::to_string(&stored)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn job_result(&self, id: JobId) -> Result<Option<JobResult>, GraphError> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT result FROM job_results WHERE job_id = ?1",
                params![id.0.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value| {
            let result = serde_json::from_str(&value)?;
            crate::evidence_store::hydrate_job_result(&self.conn, result).map_err(GraphError::from)
        })
        .transpose()
    }

    /// Evidence is linked through its immutable job to the stable task key.
    pub fn job_results_for_task(&self, task_key: &str) -> Result<Vec<JobResult>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.result FROM job_results r JOIN jobs j ON j.id = r.job_id
             WHERE j.task_key = ?1 ORDER BY r.created_at",
        )?;
        let rows = stmt.query_map(params![task_key], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            let json = row?;
            let result = serde_json::from_str(&json)?;
            crate::evidence_store::hydrate_job_result(&self.conn, result).map_err(GraphError::from)
        })
        .collect()
    }

    /// Enumerate every stored job result without hydrating evidence blobs.
    /// A malformed row is surfaced as `parsed: Err(..)` so a retention sweep
    /// can quarantine it instead of crashing on corrupt history.
    pub fn job_result_rows(&self) -> Result<Vec<JobResultRow>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.job_id, j.task_key, r.created_at, r.result
             FROM job_results r LEFT JOIN jobs j ON j.id = r.job_id
             ORDER BY r.created_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (job_id, task_key, created_at, json) = row?;
            let parsed =
                serde_json::from_str::<JobResult>(&json).map_err(|error| error.to_string());
            Ok(JobResultRow {
                job_id,
                task_key,
                created_at,
                raw: json,
                parsed,
            })
        })
        .collect()
    }

    pub fn job_result_exists(&self, id: JobId) -> Result<bool, GraphError> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM job_results WHERE job_id = ?1",
                params![id.0.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Delete one job's evidence payload. The `jobs`, `reviews`, and `events`
    /// rows are append-only history and are never touched; only the governed
    /// evidence is erased. Returns whether a row existed.
    pub fn delete_job_result(&mut self, id: JobId) -> Result<bool, GraphError> {
        let deleted = self.conn.execute(
            "DELETE FROM job_results WHERE job_id = ?1",
            params![id.0.to_string()],
        )?;
        Ok(deleted > 0)
    }

    /// Every blob digest referenced by any remaining job result. Valid rows
    /// contribute their structural references; malformed rows are scanned for
    /// `sha256:<64 hex>` tokens so corrupt history can never cause its blobs
    /// to be treated as unreferenced (conservative-safe for garbage collection).
    pub fn referenced_blob_digests(&self) -> Result<BTreeSet<String>, GraphError> {
        let mut digests = BTreeSet::new();
        for row in self.job_result_rows()? {
            match &row.parsed {
                Ok(result) => {
                    if let Some(change_set) = &result.change_set {
                        for change in &change_set.files {
                            for evidence in [change.before.as_ref(), change.after.as_ref()]
                                .into_iter()
                                .flatten()
                            {
                                if let Some(blob) = &evidence.blob {
                                    digests.insert(blob.clone());
                                }
                                digests.insert(evidence.digest.clone());
                            }
                        }
                    }
                }
                Err(_) => scan_sha256_tokens(&row.raw, &mut digests),
            }
        }
        Ok(digests)
    }

    /// Location of the content-addressed evidence store for file-backed
    /// graphs; `None` for in-memory graphs, which keep evidence inline.
    pub fn blob_store_root(&self) -> Result<Option<std::path::PathBuf>, GraphError> {
        Ok(crate::evidence_store::store_root(&self.conn)?)
    }

    pub fn record_review(&mut self, review: &Review) -> Result<TaskState, GraphError> {
        if self.task_state(&review.task_key)? != Some(TaskState::Review) {
            return Err(GraphError::ResultStateMismatch);
        }
        let job_task: String = self.conn.query_row(
            "SELECT task_key FROM jobs WHERE id = ?1 AND state = 'succeeded'",
            params![review.job_id.0.to_string()],
            |row| row.get(0),
        )?;
        if job_task != review.task_key || self.job_result(review.job_id)?.is_none() {
            return Err(GraphError::ResultStateMismatch);
        }
        self.conn.execute(
            "INSERT INTO reviews (id, task_key, job_id, decision, reviewer, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                Uuid::new_v4().to_string(),
                review.task_key,
                review.job_id.0.to_string(),
                match review.decision {
                    ReviewDecision::Approve => "approve",
                    ReviewDecision::Reject => "reject",
                },
                review.reviewer,
                review.reason,
                Utc::now().to_rfc3339(),
            ],
        )?;
        self.transition_task(
            &review.task_key,
            match review.decision {
                ReviewDecision::Approve => TaskState::Done,
                ReviewDecision::Reject => TaskState::Ready,
            },
        )
    }

    pub fn reviews_for_task(&self, task_key: &str) -> Result<Vec<Review>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT task_key, job_id, decision, reviewer, reason FROM reviews
             WHERE task_key = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![task_key], |row| {
            let id: String = row.get(1)?;
            let id = Uuid::parse_str(&id).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            let decision: String = row.get(2)?;
            Ok(Review {
                task_key: row.get(0)?,
                job_id: JobId(id),
                decision: if decision == "approve" {
                    ReviewDecision::Approve
                } else {
                    ReviewDecision::Reject
                },
                reviewer: row.get(3)?,
                reason: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(GraphError::Sqlite)
    }

    /// Persist immutable advisory review text. Drafts cannot transition task state.
    pub fn record_review_draft(&mut self, draft: &ReviewDraft) -> Result<(), GraphError> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO review_drafts
             (id, task_key, job_id, perspective, recommendation, body, agent, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                draft.id.to_string(),
                draft.task_key,
                draft.job_id.0.to_string(),
                draft.perspective.as_str(),
                match draft.recommendation {
                    ReviewDecision::Approve => "approve",
                    ReviewDecision::Reject => "reject",
                },
                draft.body,
                draft.agent,
                draft.created_at.to_rfc3339(),
            ],
        )?;
        if inserted == 1 {
            let context_key = format!("review:{}", draft.job_id.0);
            let question_id = self.ensure_discourse_question(
                &context_key,
                "Does the immutable evidence justify the proposed task decision, and what would falsify that conclusion?",
            )?;
            let turn = DiscourseTurn {
                id: draft.id,
                context_key,
                speaker: DiscourseSpeaker::SocraticPartner,
                act: match draft.perspective {
                    ReviewPerspective::Evidence => DiscourseAct::Observation,
                    ReviewPerspective::Adversarial => DiscourseAct::Challenge,
                },
                body: draft.body.clone(),
                reply_to: Some(question_id),
                created_at: draft.created_at,
            };
            self.record_discourse_turn(&turn)?;
            self.emit_event(&Event::ReviewDrafted {
                draft_id: draft.id,
                job_id: draft.job_id,
                perspective: draft.perspective.as_str().into(),
            })?;
        }
        Ok(())
    }

    pub fn review_drafts_for_job(&self, job_id: JobId) -> Result<Vec<ReviewDraft>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_key, perspective, recommendation, body, agent, created_at
             FROM review_drafts WHERE job_id = ?1 ORDER BY perspective",
        )?;
        let rows = stmt.query_map(params![job_id.0.to_string()], |row| {
            let id: String = row.get(0)?;
            let id = Uuid::parse_str(&id).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            let perspective: String = row.get(2)?;
            let recommendation: String = row.get(3)?;
            let created_at: String = row.get(6)?;
            let created_at = DateTime::parse_from_rfc3339(&created_at)
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?
                .with_timezone(&Utc);
            Ok(ReviewDraft {
                id,
                task_key: row.get(1)?,
                job_id,
                perspective: if perspective == "evidence" {
                    ReviewPerspective::Evidence
                } else {
                    ReviewPerspective::Adversarial
                },
                recommendation: if recommendation == "approve" {
                    ReviewDecision::Approve
                } else {
                    ReviewDecision::Reject
                },
                body: row.get(4)?,
                agent: row.get(5)?,
                created_at,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(GraphError::Sqlite)
    }

    /// Record the human-edited resolution. A pending review performs the
    /// authoritative transition; an already-reviewed job records retrospective
    /// learning without rewriting the historical decision or task state.
    pub fn record_review_resolution(
        &mut self,
        resolution: &ReviewResolution,
    ) -> Result<TaskState, GraphError> {
        if let Some(draft_id) = resolution.selected_draft_id {
            let count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM review_drafts
                 WHERE id = ?1 AND task_key = ?2 AND job_id = ?3",
                params![
                    draft_id.to_string(),
                    resolution.task_key,
                    resolution.job_id.0.to_string()
                ],
                |row| row.get(0),
            )?;
            if count != 1 {
                return Err(GraphError::ResultStateMismatch);
            }
        }

        let task_state = self
            .task_state(&resolution.task_key)?
            .ok_or(GraphError::ResultStateMismatch)?;
        let state = if task_state == TaskState::Review {
            let review = Review {
                task_key: resolution.task_key.clone(),
                job_id: resolution.job_id,
                decision: resolution.decision,
                reviewer: resolution.reviewer.clone(),
                reason: resolution.final_body.clone(),
            };
            self.record_review(&review)?
        } else {
            let recorded = self
                .reviews_for_task(&resolution.task_key)?
                .into_iter()
                .find(|review| review.job_id == resolution.job_id)
                .ok_or(GraphError::ResultStateMismatch)?;
            if recorded.decision != resolution.decision {
                return Err(GraphError::ResultStateMismatch);
            }
            task_state
        };
        self.conn.execute(
            "INSERT INTO review_resolutions
             (id, task_key, job_id, selected_draft_id, original_draft, final_body,
              edit_similarity, decision, reviewer, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                resolution.id.to_string(),
                resolution.task_key,
                resolution.job_id.0.to_string(),
                resolution.selected_draft_id.map(|id| id.to_string()),
                resolution.original_draft,
                resolution.final_body,
                resolution.edit_similarity,
                match resolution.decision {
                    ReviewDecision::Approve => "approve",
                    ReviewDecision::Reject => "reject",
                },
                resolution.reviewer,
                resolution.created_at.to_rfc3339(),
            ],
        )?;
        let context_key = format!("review:{}", resolution.job_id.0);
        let question_id = self.ensure_discourse_question(
            &context_key,
            "Does the immutable evidence justify the proposed task decision, and what would falsify that conclusion?",
        )?;
        let reply_to = if let Some(draft_id) = resolution.selected_draft_id {
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM discourse_turns WHERE id = ?1)",
                params![draft_id.to_string()],
                |row| row.get(0),
            )?;
            if !exists {
                let (body, perspective): (String, String) = self.conn.query_row(
                    "SELECT body, perspective FROM review_drafts WHERE id = ?1",
                    params![draft_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                self.record_discourse_turn(&DiscourseTurn {
                    id: draft_id,
                    context_key: context_key.clone(),
                    speaker: DiscourseSpeaker::SocraticPartner,
                    act: if perspective == "evidence" {
                        DiscourseAct::Observation
                    } else {
                        DiscourseAct::Challenge
                    },
                    body,
                    reply_to: Some(question_id),
                    created_at: resolution.created_at,
                })?;
            }
            draft_id
        } else {
            question_id
        };
        self.record_discourse_turn(&DiscourseTurn {
            id: resolution.id,
            context_key,
            speaker: DiscourseSpeaker::Human,
            act: DiscourseAct::Synthesis,
            body: resolution.final_body.clone(),
            reply_to: Some(reply_to),
            created_at: resolution.created_at,
        })?;
        self.emit_event(&Event::ReviewResolved {
            resolution_id: resolution.id,
            job_id: resolution.job_id,
            selected_draft_id: resolution.selected_draft_id,
        })?;
        Ok(state)
    }

    /// Human-authored resolutions are compact learning context for later reviews.
    pub fn recent_review_lessons(&self, limit: usize) -> Result<Vec<String>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT decision, final_body FROM review_resolutions
             ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(format!(
                "{}: {}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(GraphError::Sqlite)
    }

    /// Human resolutions scoped to the task being decided. This avoids
    /// treating unrelated historical prose as globally applicable policy.
    pub fn review_lessons_for_task(
        &self,
        task_key: &str,
        limit: usize,
    ) -> Result<Vec<String>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT decision, final_body FROM review_resolutions
             WHERE task_key = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![task_key, limit as i64], |row| {
            Ok(format!(
                "{}: {}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(GraphError::Sqlite)
    }

    pub fn create_node(&mut self, node: &Node) -> Result<NodeId, GraphError> {
        self.conn.execute(
            "INSERT INTO nodes (id, kind, name, payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                node.id.0.to_string(),
                node.kind.as_str(),
                node.name,
                serde_json::to_string(&node.payload).unwrap(),
                node.created_at.to_rfc3339()
            ],
        )?;
        self.emit_event(&Event::NodeCreated { node_id: node.id })?;
        Ok(node.id)
    }

    /// Update an existing node's payload by ID.
    pub fn update_node_payload(&mut self, id: NodeId, payload: Value) -> Result<(), GraphError> {
        self.conn.execute(
            "UPDATE nodes SET payload = ?1, created_at = ?2 WHERE id = ?3",
            params![
                serde_json::to_string(&payload).unwrap(),
                Utc::now().to_rfc3339(),
                id.0.to_string()
            ],
        )?;
        Ok(())
    }

    /// Create a node or update its payload if one with the same kind+name already exists.
    pub fn upsert_node_by_name(&mut self, node: &Node) -> Result<NodeId, GraphError> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM nodes WHERE kind = ?1 AND name = ?2",
                params![node.kind.as_str(), node.name],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(id_str) = existing {
            let id = NodeId(Uuid::parse_str(&id_str).map_err(|e| {
                GraphError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                ))
            })?);
            self.conn.execute(
                "UPDATE nodes SET payload = ?1, created_at = ?2 WHERE id = ?3",
                params![
                    serde_json::to_string(&node.payload).unwrap(),
                    Utc::now().to_rfc3339(),
                    id.0.to_string()
                ],
            )?;
            Ok(id)
        } else {
            self.create_node(node)
        }
    }

    /// Approve a rule node by its name (rule id). Returns true if the node was found and updated.
    pub fn approve_rule(&mut self, rule_id: &str) -> Result<bool, GraphError> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM nodes WHERE kind = 'rule' AND name = ?1",
                params![rule_id],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(id_str) = existing {
            let id = NodeId(Uuid::parse_str(&id_str).map_err(|e| {
                GraphError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                ))
            })?);
            let payload = serde_json::json!({ "approved": true });
            self.conn.execute(
                "UPDATE nodes SET payload = ?1, created_at = ?2 WHERE id = ?3",
                params![
                    serde_json::to_string(&payload).unwrap(),
                    Utc::now().to_rfc3339(),
                    id.0.to_string()
                ],
            )?;
            self.emit_event(&Event::ReviewRequested {
                review_id: id,
                task_id: id,
            })?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn get_node(&self, id: NodeId) -> Result<Option<Node>, GraphError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, kind, name, payload, created_at FROM nodes WHERE id = ?1")?;
        let node = stmt
            .query_row(params![id.0.to_string()], Self::map_node)
            .optional()?;
        Ok(node)
    }

    pub fn find_node_by_name(
        &self,
        kind: NodeKind,
        name: &str,
    ) -> Result<Option<Node>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, payload, created_at FROM nodes WHERE kind = ?1 AND name = ?2",
        )?;
        let node = stmt
            .query_row(params![kind.as_str(), name], Self::map_node)
            .optional()?;
        Ok(node)
    }

    pub fn list_nodes(&self, kind: Option<NodeKind>) -> Result<Vec<Node>, GraphError> {
        let sql = match kind {
            Some(_) => {
                "SELECT id, kind, name, payload, created_at FROM nodes WHERE kind = ?1 ORDER BY created_at"
            }
            None => "SELECT id, kind, name, payload, created_at FROM nodes ORDER BY created_at",
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = match kind {
            Some(k) => stmt.query_map(params![k.as_str()], Self::map_node)?,
            None => stmt.query_map([], Self::map_node)?,
        };
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(GraphError::Sqlite)
    }

    fn map_node(row: &rusqlite::Row<'_>) -> Result<Node, rusqlite::Error> {
        let id_str: String = row.get(0)?;
        let kind_str: String = row.get(1)?;
        let name: String = row.get(2)?;
        let payload_str: String = row.get(3)?;
        let created_at_str: String = row.get(4)?;

        let id = Uuid::parse_str(&id_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let kind = kind_str.parse().map_err(|e: String| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(ColumnParseError(e)),
            )
        })?;
        let payload: Value = serde_json::from_str(&payload_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?
            .with_timezone(&Utc);

        Ok(Node {
            id: NodeId(id),
            kind,
            name,
            payload,
            created_at,
        })
    }

    pub fn list_edges(&self) -> Result<Vec<Edge>, GraphError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, from_node, to_node, kind FROM edges")?;
        let rows = stmt.query_map([], |row| {
            let id_str: String = row.get(0)?;
            let from_str: String = row.get(1)?;
            let to_str: String = row.get(2)?;
            let kind_str: String = row.get(3)?;

            let id = Uuid::parse_str(&id_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let from = NodeId(Uuid::parse_str(&from_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?);
            let to = NodeId(Uuid::parse_str(&to_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?);
            let kind = kind_str.parse().map_err(|e: String| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(ColumnParseError(e)),
                )
            })?;

            Ok(Edge { id, from, to, kind })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(GraphError::Sqlite)
    }

    pub fn create_edge(
        &mut self,
        from: NodeId,
        to: NodeId,
        kind: EdgeKind,
    ) -> Result<Edge, GraphError> {
        let edge = Edge {
            id: Uuid::new_v4(),
            from,
            to,
            kind,
        };
        self.conn.execute(
            "INSERT INTO edges (id, from_node, to_node, kind)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                edge.id.to_string(),
                from.0.to_string(),
                to.0.to_string(),
                kind.as_str()
            ],
        )?;
        self.emit_event(&Event::EdgeCreated {
            from,
            to,
            edge_kind: kind.as_str().to_string(),
        })?;
        Ok(edge)
    }

    /// Test-only: delete a node while leaving any incident edges in place.
    /// Used to simulate corruption for diagnostic-rule tests.
    #[cfg(test)]
    pub fn test_delete_node_leave_edges(&mut self, id: NodeId) -> Result<(), GraphError> {
        self.conn.execute_batch("PRAGMA foreign_keys=OFF")?;
        self.conn
            .execute("DELETE FROM nodes WHERE id = ?1", params![id.0.to_string()])?;
        self.conn.execute_batch("PRAGMA foreign_keys=ON")?;
        Ok(())
    }

    pub fn neighbors(&self, id: NodeId) -> Result<Vec<(EdgeKind, Node)>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.kind, n.name, n.payload, n.created_at, e.kind
             FROM edges e
             JOIN nodes n ON e.to_node = n.id
             WHERE e.from_node = ?1
             UNION ALL
             SELECT n.id, n.kind, n.name, n.payload, n.created_at, e.kind
             FROM edges e
             JOIN nodes n ON e.from_node = n.id
             WHERE e.to_node = ?1",
        )?;
        let rows = stmt.query_map(params![id.0.to_string()], |row| {
            let node = Self::map_node(row)?;
            let edge_kind_str: String = row.get(5)?;
            let edge_kind = match edge_kind_str.as_str() {
                "depends_on" => EdgeKind::DependsOn,
                "implements" => EdgeKind::Implements,
                "tests" => EdgeKind::Tests,
                "reviews" => EdgeKind::Reviews,
                "deploys" => EdgeKind::Deploys,
                "learns_from" => EdgeKind::LearnsFrom,
                "contains" => EdgeKind::Contains,
                _ => panic!("unknown edge kind in db: {}", edge_kind_str),
            };
            Ok((edge_kind, node))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(GraphError::Sqlite)
    }

    /// Index a code file into the graph.
    /// Idempotent: re-indexing the same path updates the existing node and search index.
    pub fn index_code(&mut self, relative_path: &str, content: &str) -> Result<NodeId, GraphError> {
        self.index_code_with_embedding(relative_path, content, None)
    }

    /// Index a code file and optionally store a dense embedding for semantic search.
    pub fn index_code_with_embedding(
        &mut self,
        relative_path: &str,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> Result<NodeId, GraphError> {
        let tx = self.conn.transaction()?;

        let existing_id: Option<String> = tx
            .query_row(
                "SELECT id FROM nodes WHERE kind = 'code' AND name = ?1",
                params![relative_path],
                |row| row.get(0),
            )
            .optional()?;

        let id = if let Some(id_str) = existing_id {
            let id = NodeId(Uuid::parse_str(&id_str).map_err(|e| {
                GraphError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                ))
            })?);
            let payload = serde_json::json!({
                "path": relative_path,
                "lines": content.lines().count(),
            });
            tx.execute(
                "UPDATE nodes SET payload = ?1, created_at = ?2 WHERE id = ?3",
                params![
                    serde_json::to_string(&payload).unwrap(),
                    Utc::now().to_rfc3339(),
                    id.0.to_string()
                ],
            )?;

            // Remove stale search rows tied to this file.
            let stale_rowids: Vec<i64> = tx
                .prepare("SELECT rowid FROM code_index WHERE node_id = ?1")?
                .query_map(params![id.0.to_string()], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for rowid in stale_rowids {
                tx.execute("DELETE FROM code_search WHERE rowid = ?1", params![rowid])?;
            }
            tx.execute(
                "DELETE FROM code_index WHERE node_id = ?1",
                params![id.0.to_string()],
            )?;

            id
        } else {
            let payload = serde_json::json!({
                "path": relative_path,
                "lines": content.lines().count(),
            });
            let node = Node::new(NodeKind::Code, relative_path, payload);
            let id = node.id;
            tx.execute(
                "INSERT INTO nodes (id, kind, name, payload, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.0.to_string(),
                    node.kind.as_str(),
                    node.name,
                    serde_json::to_string(&node.payload).unwrap(),
                    node.created_at.to_rfc3339()
                ],
            )?;
            id
        };

        tx.execute(
            "INSERT INTO code_index (id, node_id, content) VALUES (?1, ?2, ?3)",
            params![Uuid::new_v4().to_string(), id.0.to_string(), content],
        )?;
        let rowid = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO code_search (rowid, content) VALUES (?1, ?2)",
            params![rowid, content],
        )?;

        if let Some(emb) = embedding {
            let emb_json = serde_json::to_string(emb).unwrap();
            tx.execute(
                "INSERT OR REPLACE INTO code_embeddings (node_id, embedding) VALUES (?1, ?2)",
                params![id.0.to_string(), emb_json],
            )?;
        }

        tx.commit()?;
        self.emit_event(&Event::CodeIndexed {
            path: relative_path.to_string(),
            node_id: id,
            lines: content.lines().count(),
        })?;
        Ok(id)
    }

    #[cfg(test)]
    fn test_insert_code(
        &mut self,
        relative_path: &str,
        content: &str,
    ) -> Result<NodeId, GraphError> {
        self.index_code(relative_path, content)
    }

    pub fn search_code(&self, query: &str) -> Result<Vec<(Node, String)>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.kind, n.name, n.payload, n.created_at, c.content
             FROM code_search s
             JOIN code_index c ON s.rowid = c.rowid
             JOIN nodes n ON c.node_id = n.id
             WHERE code_search MATCH ?1
             LIMIT 20",
        )?;
        let rows = stmt.query_map(params![query], |row| {
            let node = Self::map_node(row)?;
            let content: String = row.get(5)?;
            Ok((node, content))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(GraphError::Sqlite)
    }

    /// Semantic code search using dense embeddings and cosine similarity.
    pub fn semantic_search(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(Node, f32)>, GraphError> {
        let mut stmt = self
            .conn
            .prepare("SELECT n.id, n.kind, n.name, n.payload, n.created_at, e.embedding FROM code_embeddings e JOIN nodes n ON e.node_id = n.id")?;
        let rows = stmt.query_map([], |row| {
            let node = Self::map_node(row)?;
            let emb_str: String = row.get(5)?;
            let emb: Vec<f32> = serde_json::from_str(&emb_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok((node, emb))
        })?;

        let mut scored: Vec<(Node, f32)> = rows
            .filter_map(|r| r.ok())
            .filter_map(|(node, emb)| {
                crate::embed::cosine_similarity(query_embedding, &emb).map(|score| (node, score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    /// Index a markdown plan into the graph.
    ///
    /// Creates/updates a `Plan` node, a `Task` node for each plan item, and
    /// `depends_on` edges from tasks to the code nodes that best match the task
    /// description, run command, and stop condition.
    pub fn index_plan(
        &mut self,
        relative_path: &str,
        plan: &crate::plan::Plan,
    ) -> Result<NodeId, GraphError> {
        let plan_payload = serde_json::json!({
            "path": relative_path,
            "title": plan.title,
            "task_count": plan.tasks.len(),
        });
        let plan_node = Node::new(NodeKind::Plan, relative_path, plan_payload);
        let plan_id = self.upsert_node_by_name(&plan_node)?;

        // Remove stale task nodes and edges belonging to this plan.
        let prefix = format!("{}#%", relative_path);
        let stale_ids: Vec<String> = self
            .conn
            .prepare("SELECT id FROM nodes WHERE kind = 'task' AND name LIKE ?1")?
            .query_map(params![&prefix], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !stale_ids.is_empty() {
            let placeholders = stale_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let edge_sql = format!(
                "DELETE FROM edges WHERE from_node IN ({}) OR to_node IN ({})",
                placeholders, placeholders
            );
            self.conn.execute(
                &edge_sql,
                params_from_iter(stale_ids.iter().chain(stale_ids.iter())),
            )?;
            let node_sql = format!("DELETE FROM nodes WHERE id IN ({})", placeholders);
            self.conn.execute(&node_sql, params_from_iter(&stale_ids))?;
        }

        for task in &plan.tasks {
            let task_name = format!("{}#{}", relative_path, task.id);
            let task_payload = serde_json::json!({
                "description": task.description,
                "done": task.done,
                "run": task.run,
                "stop": task.stop,
            });
            let task_node = Node::new(NodeKind::Task, &task_name, task_payload);
            let task_id = self.upsert_node_by_name(&task_node)?;

            self.create_edge(plan_id, task_id, EdgeKind::Contains)?;
            self.record_event(&Event::TaskPlanned { task_id, plan_id })?;

            // Explicit file links are the strongest expression of intent.
            for file in &task.files {
                if file.starts_with("new:") {
                    continue;
                }
                match self.find_node_by_name(NodeKind::Code, file)? {
                    Some(code) => {
                        self.create_edge(task_id, code.id, EdgeKind::DependsOn)?;
                    }
                    None => {
                        eprintln!(
                            "warning: task {} references unknown code file {}",
                            task_name, file
                        );
                    }
                }
            }

            // If no explicit files are given, fall back to searching the task text.
            if task.files.is_empty() {
                let mut query_parts = vec![task.description.clone()];
                if let Some(run) = &task.run {
                    query_parts.push(run.clone());
                }
                if let Some(stop) = &task.stop {
                    query_parts.push(stop.clone());
                }
                let raw_query = query_parts.join(" ");
                let safe_query = crate::search::sanitize_query(&raw_query);
                if !safe_query.is_empty() {
                    let results = self.search_code(&safe_query)?;
                    for (code, _) in results.iter().take(3) {
                        self.create_edge(task_id, code.id, EdgeKind::DependsOn)?;
                    }
                }
            }
        }

        self.record_event(&Event::PlanIndexed {
            path: relative_path.to_string(),
            plan_id,
        })?;
        Ok(plan_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::living::{
        GovernanceEnvelope, KnowledgeLayer, RetentionPolicy, SourceRef, Transformation,
    };

    fn evidence_governance() -> GovernanceEnvelope {
        GovernanceEnvelope {
            layer: KnowledgeLayer::Observed,
            sources: vec![SourceRef {
                uri: "job://test/output".into(),
                digest: None,
            }],
            assumptions: Vec::new(),
            transformation: Transformation {
                name: "capture-job-result".into(),
                version: "1".into(),
                input_digests: Vec::new(),
            },
            owner: "foundry-tests".into(),
            retention: RetentionPolicy::Preserve {
                basis: "regression evidence".into(),
            },
        }
    }

    #[test]
    fn graph_can_create_and_retrieve_node() {
        let mut graph = Graph::open_in_memory().unwrap();
        let node = Node::new(NodeKind::Task, "bootstrap", serde_json::json!({}));
        let id = graph.create_node(&node).unwrap();
        let retrieved = graph.get_node(id).unwrap().unwrap();
        assert_eq!(retrieved.name, "bootstrap");
        assert_eq!(retrieved.kind, NodeKind::Task);
    }

    #[test]
    fn migration_schema_stores_versions_with_sha256_checksums() {
        let graph = Graph::open_in_memory().unwrap();
        let mut statement = graph
            .conn
            .prepare("PRAGMA table_info(schema_migrations)")
            .unwrap();
        let columns = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, bool>(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(
            columns.contains(&("version".into(), "INTEGER".into(), false, true)),
            "schema_migrations.version must be an INTEGER PRIMARY KEY"
        );
        assert!(
            columns.contains(&("checksum".into(), "TEXT".into(), true, false)),
            "schema_migrations.checksum must be required TEXT for a SHA-256 digest"
        );
    }

    #[test]
    fn upgrades_existing_schema_migrations_table_preserving_applied_at_and_backfilling_checksums() {
        // Capture the checksums that the current migrations are expected to produce.
        let fresh = Graph::open_in_memory().unwrap();
        let mut expected_checksums = std::collections::HashMap::new();
        {
            let mut stmt = fresh
                .conn
                .prepare("SELECT version, checksum FROM schema_migrations ORDER BY version")
                .unwrap();
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap();
            for row in rows {
                let (version, checksum) = row.unwrap();
                expected_checksums.insert(version, checksum);
            }
        }

        // Simulate a legacy graph database whose schema_migrations table only tracked
        // the applied version and timestamp.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL
            )",
            [],
        )
        .unwrap();

        let applied_at_values: Vec<&str> = vec![
            "2021-01-01T00:00:00+00:00",
            "2022-01-01T00:00:00+00:00",
            "2023-01-01T00:00:00+00:00",
            "2024-01-01T00:00:00+00:00",
            "2025-01-01T00:00:00+00:00",
        ];

        for version in 1..=5 {
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                params![version, applied_at_values[(version - 1) as usize]],
            )
            .unwrap();
        }

        let mut graph = Graph { conn };
        graph.migrate().unwrap();

        // The upgrade must add a checksum column to the existing table.
        let mut stmt = graph
            .conn
            .prepare("PRAGMA table_info(schema_migrations)")
            .unwrap();
        let columns = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, bool>(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            columns.contains(&("checksum".into(), "TEXT".into(), true, false)),
            "schema_migrations.checksum must be added as a required TEXT column"
        );

        // Every previously applied version must keep its original applied_at and have its
        // checksum backfilled from the corresponding migration.
        let mut stmt = graph
            .conn
            .prepare(
                "SELECT version, checksum, applied_at FROM schema_migrations
                 WHERE version <= 5 ORDER BY version",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(rows.len(), 5);
        for (version, checksum, applied_at) in rows {
            assert_eq!(
                expected_checksums.get(&version).unwrap(),
                &checksum,
                "checksum for version {} must be backfilled from the migration SQL",
                version
            );
            assert_eq!(
                applied_at_values[(version - 1) as usize],
                applied_at,
                "applied_at for version {} must be preserved during the upgrade",
                version
            );
        }

        assert_eq!(graph.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    }

    #[test]
    fn upgrade_preserves_non_empty_checksums_and_only_backfills_empty_legacy_checksums() {
        // A database that already has checksum storage must not have its recorded
        // checksums overwritten: empty legacy checksums are backfilled, but any
        // stored non-empty checksum is treated as authoritative.
        let fresh = Graph::open_in_memory().unwrap();
        let expected_v1_checksum: String = fresh
            .conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let expected_v2_checksum: String = fresh
            .conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 2",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let expected_v3_checksum: String = fresh
            .conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 3",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();

        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                checksum TEXT NOT NULL,
                applied_at TEXT NOT NULL
            )",
            [],
        )
        .unwrap();

        let applied_at_values: Vec<&str> = vec![
            "2021-01-01T00:00:00+00:00",
            "2022-01-01T00:00:00+00:00",
            "2023-01-01T00:00:00+00:00",
        ];

        conn.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, ?2, ?3)",
            params![1, expected_v1_checksum, applied_at_values[0]],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, ?2, ?3)",
            params![2, "", applied_at_values[1]],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, ?2, ?3)",
            params![3, expected_v3_checksum, applied_at_values[2]],
        )
        .unwrap();

        let mut graph = Graph { conn };
        graph.migrate().unwrap();

        let mut stmt = graph
            .conn
            .prepare(
                "SELECT version, checksum, applied_at FROM schema_migrations
                 WHERE version <= 3 ORDER BY version",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(rows.len(), 3);
        for (version, checksum, applied_at) in rows {
            match version {
                1 => assert_eq!(
                    checksum, expected_v1_checksum,
                    "existing non-empty checksum for version {} must not be overwritten",
                    version
                ),
                2 => assert_eq!(
                    checksum, expected_v2_checksum,
                    "empty legacy checksum for version {} must be backfilled from the migration SQL",
                    version
                ),
                3 => assert_eq!(
                    checksum, expected_v3_checksum,
                    "existing non-empty checksum for version {} must not be overwritten",
                    version
                ),
                _ => unreachable!(),
            }
            assert_eq!(
                applied_at_values[(version - 1) as usize],
                applied_at,
                "applied_at for version {} must be preserved during the upgrade",
                version
            );
        }

        assert_eq!(graph.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    }

    /// Regression: Graph::migrate and the exported MigrationStorage API must share a single
    /// source of truth for the schema_migrations table. Comparing the full table definition
    /// (name, type, nullability, default, primary-key) proves the two entry points do not
    /// maintain parallel DDL that could diverge.
    #[test]
    fn graph_and_migration_storage_use_identical_schema_migrations_table() {
        use crate::migration_storage::MigrationStorage;

        let graph = Graph::open_in_memory().unwrap();
        let storage = MigrationStorage::open_in_memory().unwrap();

        fn table_info(conn: &Connection) -> Vec<(String, String, bool, Option<String>, bool)> {
            let mut stmt = conn
                .prepare("PRAGMA table_info(schema_migrations)")
                .unwrap();
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        }

        assert_eq!(
            table_info(&graph.conn),
            table_info(storage.connection()),
            "Graph::migrate and MigrationStorage must use the same schema_migrations table definition"
        );
    }

    /// Legacy schema_migrations rows may represent a missing checksum as either NULL or the
    /// empty string. The upgrade must backfill both representations from the migration SQL.
    #[test]
    fn upgrade_backfills_null_and_empty_legacy_checksums() {
        let fresh = Graph::open_in_memory().unwrap();
        let expected_v1_checksum: String = fresh
            .conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let expected_v2_checksum: String = fresh
            .conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 2",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();

        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                checksum TEXT,
                applied_at TEXT NOT NULL
            )",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, ?2, ?3)",
            params![1, None::<String>, "2021-01-01T00:00:00+00:00"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, ?2, ?3)",
            params![2, "", "2022-01-01T00:00:00+00:00"],
        )
        .unwrap();

        let mut graph = Graph { conn };
        graph.migrate().unwrap();

        let v1_checksum: String = graph
            .conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let v2_checksum: String = graph
            .conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            v1_checksum, expected_v1_checksum,
            "NULL legacy checksum must be backfilled from the migration SQL"
        );
        assert_eq!(
            v2_checksum, expected_v2_checksum,
            "empty-string legacy checksum must be backfilled from the migration SQL"
        );
    }

    /// A stored checksum must be the SHA-256 digest of the migration content that was actually
    /// applied. If a legacy row already contains a checksum that does not match the known
    /// migration SQL, migration must fail rather than silently accept a mismatched digest.
    #[test]
    fn migrate_rejects_stored_checksum_that_does_not_match_migration_sql() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                checksum TEXT NOT NULL,
                applied_at TEXT NOT NULL
            )",
            [],
        )
        .unwrap();

        let wrong_checksum = "0000000000000000000000000000000000000000000000000000000000000000";
        conn.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, ?2, ?3)",
            params![1, wrong_checksum, "2021-01-01T00:00:00+00:00"],
        )
        .unwrap();

        let mut graph = Graph { conn };
        let result = graph.migrate();
        assert!(
            result.is_err(),
            "migrate must fail when a stored checksum does not match the SHA-256 of the migration SQL"
        );
    }

    /// If a legacy database records a schema version for which no migration content is known,
    /// the upgrade cannot backfill a checksum and must report an error instead of silently
    /// leaving the row in an inconsistent state.
    #[test]
    fn migrate_fails_when_legacy_version_has_no_migration_content() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                checksum TEXT,
                applied_at TEXT NOT NULL
            )",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, ?2, ?3)",
            params![100, None::<String>, "2021-01-01T00:00:00+00:00"],
        )
        .unwrap();

        let mut graph = Graph { conn };
        let result = graph.migrate();
        assert!(
            result.is_err(),
            "migrate must fail when a legacy schema_migrations row references a version with no known migration content"
        );
    }

    /// Regression (reviewer feedback): upgrading a legacy database in place must produce a
    /// schema_migrations table that is identical to the canonical definition shared with
    /// MigrationStorage. An ALTER TABLE upgrade can silently diverge in column order and
    /// default values, so this test forces a single source of truth for the DDL.
    #[test]
    fn upgraded_legacy_database_uses_canonical_schema_migrations_definition() {
        use crate::migration_storage::MigrationStorage;

        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL
            )",
            [],
        )
        .unwrap();

        for version in 1..=3 {
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                params![version, format!("{}-01-01T00:00:00+00:00", 2020 + version)],
            )
            .unwrap();
        }

        let mut graph = Graph { conn };
        graph.migrate().unwrap();

        fn table_info(conn: &Connection) -> Vec<(String, String, bool, Option<String>, bool)> {
            let mut stmt = conn
                .prepare("PRAGMA table_info(schema_migrations)")
                .unwrap();
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        }

        let storage = MigrationStorage::open_in_memory().unwrap();
        assert_eq!(
            table_info(&graph.conn),
            table_info(storage.connection()),
            "upgrading a legacy database must produce the same schema_migrations table definition as MigrationStorage"
        );
    }

    /// Migration checksums must be backfilled from a canonical migration registry that is
    /// independent of Graph::migrate's internal loop. This prevents the graph and any other
    /// consumers (such as MigrationStorage) from computing divergent digests for the same
    /// migration version.
    #[test]
    fn graph_backfills_legacy_checksums_from_canonical_migration_registry() {
        let expected_v1_checksum = crate::migration_registry::checksum_for(1)
            .expect("version 1 must be present in the canonical migration registry");

        // Simulate a legacy graph database whose schema_migrations table only tracked
        // the applied version and timestamp.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![1, "2021-01-01T00:00:00+00:00"],
        )
        .unwrap();

        let mut graph = Graph { conn };
        graph.migrate().unwrap();

        let checksum: String = graph
            .conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            checksum, expected_v1_checksum,
            "backfilled checksum must come from the canonical migration registry"
        );
    }

    #[test]
    fn graph_can_index_and_search_code() {
        let mut graph = Graph::open_in_memory().unwrap();
        graph
            .test_insert_code("src/lib.rs", "pub struct Graph;")
            .unwrap();
        let results = graph.search_code("Graph").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.name, "src/lib.rs");
    }

    #[test]
    fn graph_index_is_idempotent() {
        let mut graph = Graph::open_in_memory().unwrap();
        let id1 = graph.index_code("src/lib.rs", "first").unwrap();
        let id2 = graph.index_code("src/lib.rs", "second Graph").unwrap();
        assert_eq!(id1, id2);
        let results = graph.search_code("Graph").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.name, "src/lib.rs");
    }

    #[test]
    fn graph_can_create_edges() {
        let mut graph = Graph::open_in_memory().unwrap();
        let a = Node::new(NodeKind::Task, "a", serde_json::json!({}));
        let b = Node::new(NodeKind::Code, "b", serde_json::json!({}));
        let aid = graph.create_node(&a).unwrap();
        let bid = graph.create_node(&b).unwrap();
        graph.create_edge(aid, bid, EdgeKind::Implements).unwrap();
        let neighbors = graph.neighbors(aid).unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].1.name, "b");
    }

    #[test]
    fn graph_rejects_edges_to_missing_nodes() {
        let mut graph = Graph::open_in_memory().unwrap();
        let node = Node::new(NodeKind::Task, "real", serde_json::json!({"ok": true}));
        let node_id = graph.create_node(&node).unwrap();

        let result = graph.create_edge(node_id, NodeId::new(), EdgeKind::DependsOn);

        assert!(matches!(
            result,
            Err(GraphError::Sqlite(rusqlite::Error::SqliteFailure(_, _)))
        ));
    }

    #[test]
    fn job_persistence_is_idempotent_and_validates_transitions() {
        let mut graph = Graph::open_in_memory().unwrap();
        let task_key = "plans/test.plan.md#task-1";
        assert_eq!(
            graph
                .initialize_task_state(task_key, TaskState::Proposed)
                .unwrap(),
            TaskState::Proposed
        );
        assert_eq!(
            graph.transition_task(task_key, TaskState::Ready).unwrap(),
            TaskState::Ready
        );

        let first = graph.create_job(task_key, "attempt-1").unwrap();
        let repeated = graph.create_job(task_key, "attempt-1").unwrap();
        assert_eq!(first, repeated);
        assert_eq!(
            graph.transition_job(first.id, JobState::Running).unwrap(),
            JobState::Running
        );
        assert_eq!(
            graph.transition_job(first.id, JobState::Succeeded).unwrap(),
            JobState::Succeeded
        );
        assert!(graph.transition_job(first.id, JobState::Running).is_err());
    }

    #[test]
    fn job_persistence_keeps_original_task_for_reused_key() {
        let mut graph = Graph::open_in_memory().unwrap();
        let original = graph.create_job("task-a", "same-operation").unwrap();
        let repeated = graph.create_job("task-b", "same-operation").unwrap();

        assert_eq!(original.id, repeated.id);
        assert_eq!(repeated.task_key, "task-a");
    }

    #[test]
    fn job_result_is_immutable_and_matches_terminal_state() {
        let mut graph = Graph::open_in_memory().unwrap();
        let job = graph.create_job("task-a", "evidence-1").unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();
        let mut result =
            JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap();
        result.stdout = "first".into();
        graph.record_job_result(&result).unwrap();

        result.stdout = "replacement".into();
        graph.record_job_result(&result).unwrap();
        assert_eq!(graph.job_result(job.id).unwrap().unwrap().stdout, "first");
        assert_eq!(graph.job_results_for_task("task-a").unwrap().len(), 1);
        assert!(graph.job_results_for_task("task-b").unwrap().is_empty());

        let mismatched = JobResult::new(job.id, JobState::Failed, evidence_governance()).unwrap();
        assert!(matches!(
            graph.record_job_result(&mismatched),
            Err(GraphError::ResultStateMismatch)
        ));
    }

    #[test]
    fn delete_job_result_erases_only_the_evidence_payload() {
        let mut graph = Graph::open_in_memory().unwrap();
        let job = graph.create_job("task-a", "delete-1").unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();
        graph
            .record_job_result(
                &JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap(),
            )
            .unwrap();
        let events_before: i64 = graph
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();

        assert!(graph.job_result_exists(job.id).unwrap());
        assert!(graph.delete_job_result(job.id).unwrap());
        assert!(!graph.job_result_exists(job.id).unwrap());
        assert!(graph.job_result(job.id).unwrap().is_none());
        assert!(!graph.delete_job_result(job.id).unwrap());

        let jobs_remaining: i64 = graph
            .conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(jobs_remaining, 1, "jobs are append-only history");
        let events_after: i64 = graph
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            events_after, events_before,
            "deletion emits no event itself"
        );
    }

    #[test]
    fn job_result_rows_quarantine_malformed_history_without_crashing() {
        let mut graph = Graph::open_in_memory().unwrap();
        let job = graph.create_job("task-a", "rows-1").unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();
        graph
            .record_job_result(
                &JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap(),
            )
            .unwrap();
        let corrupt_job = graph.create_job("task-b", "rows-corrupt").unwrap();
        graph
            .transition_job(corrupt_job.id, JobState::Running)
            .unwrap();
        graph
            .transition_job(corrupt_job.id, JobState::Failed)
            .unwrap();
        graph
            .conn
            .execute(
                "INSERT INTO job_results (job_id, result, created_at)
                 VALUES (?1, '{\"corrupt\": tru', '2026-01-01T00:00:00Z')",
                params![corrupt_job.id.0.to_string()],
            )
            .unwrap();

        let rows = graph.job_result_rows().unwrap();
        assert_eq!(rows.len(), 2);
        let valid = rows.iter().find(|row| row.parsed.is_ok()).unwrap();
        assert_eq!(valid.task_key.as_deref(), Some("task-a"));
        let corrupt = rows.iter().find(|row| row.parsed.is_err()).unwrap();
        assert_eq!(corrupt.job_id, corrupt_job.id.0.to_string());
        assert_eq!(corrupt.task_key.as_deref(), Some("task-b"));
    }

    #[test]
    fn referenced_blob_digests_include_malformed_rows_conservatively() {
        let mut graph = Graph::open_in_memory().unwrap();
        let job = graph.create_job("task-a", "digests-1").unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();
        let structural = format!("sha256:{}", "1".repeat(64));
        let mut result =
            JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap();
        result.change_set = Some(crate::ChangeSet {
            base_revision: "sha256:base".into(),
            patch_digest: "sha256:patch".into(),
            files: vec![crate::ChangedFile {
                path: "a.txt".into(),
                status: crate::ChangeStatus::Added,
                before: None,
                after: Some(crate::FileEvidence {
                    digest: structural.clone(),
                    bytes: b"a".to_vec(),
                    blob: None,
                    executable: false,
                }),
            }],
        });
        graph.record_job_result(&result).unwrap();
        let scanned = format!("sha256:{}", "a".repeat(64));
        let corrupt_job = graph.create_job("task-b", "digests-corrupt").unwrap();
        graph
            .transition_job(corrupt_job.id, JobState::Running)
            .unwrap();
        graph
            .transition_job(corrupt_job.id, JobState::Failed)
            .unwrap();
        graph
            .conn
            .execute(
                "INSERT INTO job_results (job_id, result, created_at)
                 VALUES (?1, ?2, '2026-01-01T00:00:00Z')",
                params![
                    corrupt_job.id.0.to_string(),
                    format!("{{\"broken\": \"{scanned}\"")
                ],
            )
            .unwrap();

        let digests = graph.referenced_blob_digests().unwrap();
        assert!(digests.contains(&structural), "structural reference kept");
        assert!(
            digests.contains(&scanned),
            "corrupt row's digests must never be treated as unreferenced"
        );
    }

    #[test]
    fn blob_store_root_is_only_present_for_file_backed_graphs() {
        let graph = Graph::open_in_memory().unwrap();
        assert!(graph.blob_store_root().unwrap().is_none());

        let root = std::env::temp_dir().join(format!("foundry-blob-root-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let file_backed = Graph::open(&root.join("db.sqlite")).unwrap();
        let store = file_backed.blob_store_root().unwrap().unwrap();
        assert!(store.ends_with("blobs/sha256"));
        drop(file_backed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_backed_graph_externalizes_and_verifies_content_evidence() {
        use sha2::{Digest, Sha256};

        let root = std::env::temp_dir().join(format!("foundry-blobs-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("db.sqlite");
        let mut graph = Graph::open(&database).unwrap();
        let job = graph.create_job("task-a", "blob-evidence").unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();
        let bytes = b"large distinctive staged content".to_vec();
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        let mut result =
            JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap();
        result.change_set = Some(crate::ChangeSet {
            base_revision: "sha256:base".into(),
            patch_digest: "sha256:patch".into(),
            files: vec![crate::ChangedFile {
                path: "result.txt".into(),
                status: crate::ChangeStatus::Added,
                before: None,
                after: Some(crate::FileEvidence {
                    digest: digest.clone(),
                    bytes: bytes.clone(),
                    blob: None,
                    executable: false,
                }),
            }],
        });
        graph.record_job_result(&result).unwrap();

        let raw: String = graph
            .conn
            .query_row(
                "SELECT result FROM job_results WHERE job_id = ?1",
                params![job.id.0.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(raw.contains("\"bytes\":[]"));
        assert!(raw.contains(&format!("\"blob\":\"{digest}\"")));
        let object = root
            .join("blobs/sha256")
            .join(digest.trim_start_matches("sha256:"));
        assert_eq!(std::fs::read(&object).unwrap(), bytes);

        let hydrated = graph.job_result(job.id).unwrap().unwrap();
        assert_eq!(
            hydrated.change_set.as_ref().unwrap().files[0]
                .after
                .as_ref()
                .unwrap()
                .bytes,
            b"large distinctive staged content"
        );
        std::fs::remove_file(&object).unwrap();
        assert!(graph.job_result(job.id).is_err());
        drop(graph);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn review_decision_is_recorded_before_task_transition() {
        let mut graph = Graph::open_in_memory().unwrap();
        graph
            .initialize_task_state("task-a", TaskState::Ready)
            .unwrap();
        graph.transition_task("task-a", TaskState::Running).unwrap();
        let job = graph.create_job("task-a", "review-1").unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();
        graph
            .record_job_result(
                &JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap(),
            )
            .unwrap();
        graph.transition_task("task-a", TaskState::Review).unwrap();
        let review = Review {
            task_key: "task-a".into(),
            job_id: job.id,
            decision: ReviewDecision::Approve,
            reviewer: "human@example.test".into(),
            reason: "evidence is sufficient".into(),
        };
        assert_eq!(graph.record_review(&review).unwrap(), TaskState::Done);
        assert_eq!(graph.reviews_for_task("task-a").unwrap(), vec![review]);
    }

    #[test]
    fn advisory_drafts_require_a_human_resolution_to_transition_state() {
        let mut graph = Graph::open_in_memory().unwrap();
        graph
            .initialize_task_state("task-reviewed", TaskState::Ready)
            .unwrap();
        graph
            .transition_task("task-reviewed", TaskState::Running)
            .unwrap();
        let job = graph.create_job("task-reviewed", "draft-review-1").unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();
        graph
            .record_job_result(
                &JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap(),
            )
            .unwrap();
        graph
            .transition_task("task-reviewed", TaskState::Review)
            .unwrap();

        let evidence = ReviewDraft {
            id: Uuid::new_v4(),
            task_key: "task-reviewed".into(),
            job_id: job.id,
            perspective: ReviewPerspective::Evidence,
            recommendation: ReviewDecision::Approve,
            body: "Tests support approval".into(),
            agent: "test-agent".into(),
            created_at: Utc::now(),
        };
        let adversarial = ReviewDraft {
            id: Uuid::new_v4(),
            perspective: ReviewPerspective::Adversarial,
            recommendation: ReviewDecision::Reject,
            body: "Compatibility evidence is missing".into(),
            ..evidence.clone()
        };
        graph.record_review_draft(&evidence).unwrap();
        graph.record_review_draft(&adversarial).unwrap();
        assert_eq!(
            graph.task_state("task-reviewed").unwrap(),
            Some(TaskState::Review)
        );
        assert_eq!(graph.review_drafts_for_job(job.id).unwrap().len(), 2);

        let resolution = ReviewResolution {
            id: Uuid::new_v4(),
            task_key: "task-reviewed".into(),
            job_id: job.id,
            selected_draft_id: Some(adversarial.id),
            original_draft: Some(adversarial.body.clone()),
            final_body: "Reject until compatibility is tested".into(),
            edit_similarity: Some(0.5),
            decision: ReviewDecision::Reject,
            reviewer: "human@example.test".into(),
            created_at: Utc::now(),
        };
        assert_eq!(
            graph.record_review_resolution(&resolution).unwrap(),
            TaskState::Ready
        );
        assert!(
            graph
                .recent_review_lessons(1)
                .unwrap()
                .first()
                .unwrap()
                .contains("compatibility")
        );
        let discourse = graph
            .discourse_for_context(&format!("review:{}", job.id.0))
            .unwrap();
        assert_eq!(discourse.len(), 4);
        assert_eq!(discourse[0].act, DiscourseAct::Question);
        assert_eq!(discourse[1].speaker, DiscourseSpeaker::SocraticPartner);
        assert_eq!(discourse[2].speaker, DiscourseSpeaker::SocraticPartner);
        assert_eq!(discourse[3].speaker, DiscourseSpeaker::Human);
        assert_eq!(discourse[3].act, DiscourseAct::Synthesis);
        assert_eq!(discourse[3].reply_to, Some(adversarial.id));
    }

    #[test]
    fn discourse_replies_must_remain_in_their_shared_context() {
        let mut graph = Graph::open_in_memory().unwrap();
        let question = DiscourseTurn::new(
            "proposal:one",
            DiscourseSpeaker::Human,
            DiscourseAct::Question,
            "What outcome are we trying to improve?",
            None,
        );
        graph.record_discourse_turn(&question).unwrap();
        let answer = DiscourseTurn::new(
            "proposal:one",
            DiscourseSpeaker::SocraticPartner,
            DiscourseAct::Synthesis,
            "The observed bottleneck is review latency.",
            Some(question.id),
        );
        graph.record_discourse_turn(&answer).unwrap();

        let cross_context = DiscourseTurn::new(
            "proposal:two",
            DiscourseSpeaker::Human,
            DiscourseAct::Challenge,
            "Does that evidence apply here?",
            Some(answer.id),
        );
        assert!(matches!(
            graph.record_discourse_turn(&cross_context),
            Err(GraphError::InvalidDiscourse(_))
        ));

        assert_eq!(
            graph.discourse_for_context("proposal:one").unwrap(),
            vec![question, answer]
        );
    }

    #[test]
    fn retrospective_resolution_preserves_the_recorded_rejection_and_adds_learning() {
        let mut graph = Graph::open_in_memory().unwrap();
        graph
            .initialize_task_state("task-retrospective", TaskState::Ready)
            .unwrap();
        graph
            .transition_task("task-retrospective", TaskState::Running)
            .unwrap();
        let job = graph
            .create_job("task-retrospective", "retrospective-1")
            .unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();
        graph
            .record_job_result(
                &JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap(),
            )
            .unwrap();
        graph
            .transition_task("task-retrospective", TaskState::Review)
            .unwrap();
        graph
            .record_review(&Review {
                task_key: "task-retrospective".into(),
                job_id: job.id,
                decision: ReviewDecision::Reject,
                reviewer: "human@example.test".into(),
                reason: "The original rejection".into(),
            })
            .unwrap();

        let draft = ReviewDraft {
            id: Uuid::new_v4(),
            task_key: "task-retrospective".into(),
            job_id: job.id,
            perspective: ReviewPerspective::Adversarial,
            recommendation: ReviewDecision::Reject,
            body: "The checksum overwrite erases tampering evidence".into(),
            agent: "test-agent".into(),
            created_at: Utc::now(),
        };
        graph.record_review_draft(&draft).unwrap();
        let resolution = ReviewResolution {
            id: Uuid::new_v4(),
            task_key: "task-retrospective".into(),
            job_id: job.id,
            selected_draft_id: Some(draft.id),
            original_draft: Some(draft.body.clone()),
            final_body: "Never overwrite a stored non-empty checksum".into(),
            edit_similarity: Some(0.4),
            decision: ReviewDecision::Reject,
            reviewer: "human@example.test".into(),
            created_at: Utc::now(),
        };

        assert_eq!(
            graph.record_review_resolution(&resolution).unwrap(),
            TaskState::Ready
        );
        assert_eq!(
            graph.task_state("task-retrospective").unwrap(),
            Some(TaskState::Ready)
        );
        assert_eq!(
            graph.reviews_for_task("task-retrospective").unwrap().len(),
            1,
            "retrospective learning must not duplicate the authoritative review"
        );
        assert!(
            graph
                .recent_review_lessons(1)
                .unwrap()
                .first()
                .unwrap()
                .contains("Never overwrite")
        );

        let conflicting = ReviewResolution {
            id: Uuid::new_v4(),
            decision: ReviewDecision::Approve,
            ..resolution
        };
        assert!(graph.record_review_resolution(&conflicting).is_err());
    }

    #[test]
    fn rebuild_preserves_rule_approvals() {
        let mut graph = Graph::open_in_memory().unwrap();
        let rule = Node::new(
            NodeKind::Rule,
            "protected_rule",
            serde_json::json!({"approved": true}),
        );
        graph.create_node(&rule).unwrap();
        graph
            .index_code("src/lib.rs", "pub fn example() {}")
            .unwrap();

        graph.truncate_derived().unwrap();

        let preserved = graph
            .find_node_by_name(NodeKind::Rule, "protected_rule")
            .unwrap()
            .unwrap();
        assert_eq!(preserved.payload["approved"], true);
        assert!(graph.list_nodes(Some(NodeKind::Code)).unwrap().is_empty());
    }

    #[test]
    fn graph_emits_events() {
        let mut graph = Graph::open_in_memory().unwrap();
        graph.index_code("src/x.rs", "fn x() {}").unwrap();
        let events = graph.events(10).unwrap();
        assert!(!events.is_empty());
        match &events[0].1 {
            Event::CodeIndexed { path, .. } => assert_eq!(path, "src/x.rs"),
            other => panic!("expected CodeIndexed, got {:?}", other),
        }
    }

    #[test]
    fn graph_records_task_lifecycle_events() {
        let mut graph = Graph::open_in_memory().unwrap();
        let task = Node::new(NodeKind::Task, "task-1", serde_json::json!({"done": false}));
        let task_id = graph.create_node(&task).unwrap();

        graph
            .record_event(&Event::TaskStarted {
                task_id,
                description: "do work".to_string(),
            })
            .unwrap();
        graph
            .record_event(&Event::TaskCompleted {
                task_id,
                description: "do work".to_string(),
            })
            .unwrap();

        let events = graph.events(2).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1.kind(), "task_completed");
        assert_eq!(events[1].1.kind(), "task_started");
    }
}
