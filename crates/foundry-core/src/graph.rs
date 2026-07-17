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

pub const LATEST_SCHEMA_VERSION: i64 = 9;

mod job_store;
mod review_store;

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

/// Outcome of re-verifying one stored migration checksum against the
/// canonical registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationChecksumStatus {
    /// Stored checksum equals the canonical SHA-256 of the registry SQL.
    Verified,
    /// Stored checksum disagrees with the canonical registry — the row or
    /// the registry was altered after application.
    Mismatch { stored: String },
    /// The version is not in this binary's registry (database written by a
    /// newer build, or a fabricated row).
    UnknownVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationChecksumReport {
    pub version: i64,
    pub status: MigrationChecksumStatus,
}

/// Joinability of append-only event history against the current node graph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventReferenceIntegrity {
    pub events_scanned: usize,
    pub events_with_missing_references: usize,
    pub missing_node_references: usize,
    /// Task events that remain narratable through a stable task key even
    /// though one of their historical node UUIDs is no longer present.
    pub narratable_by_task_key: usize,
    /// Events with missing node UUIDs and no durable identity fallback.
    pub unresolvable_events: usize,
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
    #[error("JSON encoding or stored JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
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
    #[error(
        "cannot migrate task key {old_key:?} to {new_key:?}: the target already has lifecycle state"
    )]
    TaskKeyMigrationConflict { old_key: String, new_key: String },
    #[error("corrupt stored event: {detail}")]
    CorruptStoredEvent { detail: String },
    #[error("lost the race transitioning {key}: it no longer holds state {expected}")]
    StaleTransition { key: String, expected: String },
    #[error("unknown edge kind stored in database: {kind}")]
    UnknownEdgeKind { kind: String },
    #[error("could not checkpoint the WAL: another connection is holding it open")]
    WalCheckpointBlocked,
    #[error("invalid database snapshot: {0}")]
    InvalidSnapshot(String),
    #[error("invalid embedding: {0}")]
    InvalidEmbedding(String),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "idempotency key {key:?} belongs to task {existing_task:?}, not requested task {requested_task:?}"
    )]
    IdempotencyKeyConflict {
        key: String,
        existing_task: String,
        requested_task: String,
    },
    #[error(
        "task {task_key:?} and job {job_id:?} already have a pending human review resolution; resume it before recording another decision"
    )]
    PendingReviewResolution { task_key: String, job_id: JobId },
    #[error("job {job_id:?} already has different immutable result evidence")]
    JobResultConflict { job_id: JobId },
    #[error("task {task_key:?} is {current:?}, expected {expected:?}")]
    TaskStateMismatch {
        task_key: String,
        current: TaskState,
        expected: TaskState,
    },
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
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
        self.conn.busy_timeout(std::time::Duration::from_secs(5))?;
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
                Ok((
                    *version,
                    crate::migration_registry::checksum_for(*version)
                        .ok_or(GraphError::UnknownMigration { version: *version })?,
                ))
            })
            .collect::<Result<_, GraphError>>()?;

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
                    .ok_or(GraphError::UnknownMigration { version })?
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

    fn in_immediate_transaction<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T, GraphError>,
    ) -> Result<T, GraphError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match operation(self) {
            Ok(value) => {
                if let Err(error) = self.conn.execute_batch("COMMIT") {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    tracing::error!(error = %error, "sqlite transaction commit failed");
                    return Err(GraphError::Sqlite(error));
                }
                Ok(value)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                tracing::warn!(error = %error, "sqlite transaction rolled back");
                Err(error)
            }
        }
    }

    /// Persist an event to the graph's event log.
    pub fn record_event(&mut self, event: &Event) -> Result<(), GraphError> {
        self.emit_event(event)
    }

    fn emit_event(&mut self, event: &Event) -> Result<(), GraphError> {
        let payload = serde_json::to_string(event)?;
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
    /// rule approvals, and plan/task identity tombstones remain.
    /// After this, the caller should re-index the codebase.
    pub fn truncate_derived(&mut self) -> Result<(), GraphError> {
        self.conn.execute("DELETE FROM code_search", [])?;
        self.conn.execute("DELETE FROM code_index", [])?;
        self.conn.execute("DELETE FROM code_embeddings", [])?;
        self.conn.execute("DELETE FROM edges", [])?;
        let historical_nodes = {
            let mut statement = self
                .conn
                .prepare("SELECT id, payload FROM nodes WHERE kind IN ('plan', 'task')")?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (id, raw_payload) in historical_nodes {
            let mut payload: Value = serde_json::from_str(&raw_payload)?;
            if let Some(object) = payload.as_object_mut() {
                object.insert("removed".to_string(), Value::Bool(true));
            } else {
                payload = serde_json::json!({ "removed": true });
            }
            let id = NodeId(Uuid::parse_str(&id).map_err(|error| {
                GraphError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                ))
            })?);
            self.update_node_payload(id, payload)?;
        }
        self.conn.execute(
            "DELETE FROM nodes WHERE kind NOT IN ('rule', 'plan', 'task')",
            [],
        )?;
        Ok(())
    }

    /// Fold every committed WAL transaction into the main database file so
    /// that a bare copy of that file contains all committed history. Closing
    /// a connection only checkpoints when it is the last one anywhere, so
    /// callers about to copy the file must checkpoint explicitly.
    pub fn checkpoint_wal(&self) -> Result<(), GraphError> {
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            return Err(GraphError::WalCheckpointBlocked);
        }
        Ok(())
    }

    /// Restore a prevalidated SQLite snapshot through SQLite's online backup
    /// API. Validation happens against an in-memory copy before the
    /// destination connection is touched, so a corrupt or incompatible file
    /// cannot partially replace the live graph. The backup API performs the
    /// replacement as a SQLite transaction and cooperates with WAL mode.
    pub fn restore_from_snapshot(&mut self, snapshot: &Path) -> Result<(), GraphError> {
        let source =
            Connection::open_with_flags(snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::require_integrity(&source, snapshot)?;
        let foundry_schema: bool = source.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = 'schema_migrations'
            )",
            [],
            |row| row.get(0),
        )?;
        if !foundry_schema {
            return Err(GraphError::InvalidSnapshot(format!(
                "{} is SQLite but not a Foundry graph",
                snapshot.display()
            )));
        }

        // Prove that the snapshot is a Foundry graph this binary can migrate
        // and open, without modifying the snapshot itself.
        let mut validation = Connection::open_in_memory()?;
        {
            let backup = rusqlite::backup::Backup::new(&source, &mut validation)?;
            backup.run_to_completion(128, std::time::Duration::from_millis(5), None)?;
        }
        let mut validation_graph = Self { conn: validation };
        validation_graph.migrate()?;
        Self::require_integrity(&validation_graph.conn, snapshot)?;
        validation_graph.verify_migration_checksums()?;
        drop(validation_graph);

        {
            let backup = rusqlite::backup::Backup::new(&source, &mut self.conn)?;
            backup.run_to_completion(128, std::time::Duration::from_millis(5), None)?;
        }
        self.migrate()?;
        Self::require_integrity(&self.conn, snapshot)
    }

    fn require_integrity(connection: &Connection, source: &Path) -> Result<(), GraphError> {
        let result: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if result != "ok" {
            return Err(GraphError::InvalidSnapshot(format!(
                "{} failed integrity_check: {result}",
                source.display()
            )));
        }
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

    /// Re-verify every stored migration checksum against the canonical
    /// registry. `migrate` validates on write; this is the read-side audit
    /// for `doctor`, catching tampered rows and databases written by a newer
    /// binary whose migrations this build does not know.
    pub fn verify_migration_checksums(&self) -> Result<Vec<MigrationChecksumReport>, GraphError> {
        let mut stmt = self
            .conn
            .prepare("SELECT version, checksum FROM schema_migrations ORDER BY version")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (version, stored) = row?;
            let status = match crate::migration_registry::checksum_for(version) {
                None => MigrationChecksumStatus::UnknownVersion,
                Some(canonical) if canonical == stored => MigrationChecksumStatus::Verified,
                Some(_) => MigrationChecksumStatus::Mismatch { stored },
            };
            Ok(MigrationChecksumReport { version, status })
        })
        .collect()
    }

    /// One corrupt row must surface as an error a caller can route, not a
    /// panic that takes down `doctor` — the tool you would reach for when a
    /// row is corrupt. Rows written by a newer binary (unknown event kinds)
    /// fail the same recoverable way.
    pub fn events(&self, limit: usize) -> Result<Vec<(DateTime<Utc>, Event)>, GraphError> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload, created_at FROM events ORDER BY created_at DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (payload, ts) = row?;
            let event: Event =
                serde_json::from_str(&payload).map_err(|error| GraphError::CorruptStoredEvent {
                    detail: format!("undecodable payload: {error}"),
                })?;
            let created_at = DateTime::parse_from_rfc3339(&ts)
                .map_err(|error| GraphError::CorruptStoredEvent {
                    detail: format!("invalid timestamp {ts:?}: {error}"),
                })?
                .with_timezone(&Utc);
            Ok((created_at, event))
        })
        .collect()
    }

    /// Audit every event's node UUIDs against the current graph, while
    /// separately counting task events that retain a durable key.
    pub fn event_reference_integrity(&self) -> Result<EventReferenceIntegrity, GraphError> {
        let mut node_statement = self.conn.prepare("SELECT id FROM nodes")?;
        let node_ids = node_statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<std::collections::HashSet<_>, _>>()?;

        let mut event_statement = self.conn.prepare("SELECT payload FROM events")?;
        let payloads = event_statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut report = EventReferenceIntegrity {
            events_scanned: payloads.len(),
            ..Default::default()
        };
        for payload in payloads {
            let event: Event =
                serde_json::from_str(&payload).map_err(|error| GraphError::CorruptStoredEvent {
                    detail: format!("undecodable payload: {error}"),
                })?;
            let missing = event
                .node_references()
                .into_iter()
                .filter(|id| !node_ids.contains(&id.0.to_string()))
                .count();
            if missing == 0 {
                continue;
            }
            report.events_with_missing_references += 1;
            report.missing_node_references += missing;
            if event.durable_task_key().is_some() {
                report.narratable_by_task_key += 1;
            } else {
                report.unresolvable_events += 1;
            }
        }
        Ok(report)
    }

    /// Append an immutable, reply-linked turn to a Socratic discourse.
    pub fn record_discourse_turn(&mut self, turn: &DiscourseTurn) -> Result<(), GraphError> {
        self.in_immediate_transaction(|graph| graph.record_discourse_turn_inner(turn))
    }

    fn record_discourse_turn_inner(&mut self, turn: &DiscourseTurn) -> Result<(), GraphError> {
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
        self.in_immediate_transaction(|graph| {
            graph.ensure_discourse_question_inner(context_key, question)
        })
    }

    fn ensure_discourse_question_inner(
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
        self.record_discourse_turn_inner(&turn)?;
        Ok(id)
    }

    /// Every task key with durable history under the given plan prefix
    /// (`"plans/foo.plan.md#"`). The state is `None` for keys that appear
    /// in history tables (jobs, reviews, drafts, resolutions) without a
    /// lifecycle row — raw repairs have produced exactly that shape, and
    /// reconciliation must see those keys too. Read model for plan/graph
    /// reconciliation; requires no SQL from operators.
    pub fn task_keys_with_state(
        &self,
        plan_prefix: &str,
    ) -> Result<Vec<(String, Option<TaskState>)>, GraphError> {
        let pattern = format!("{}%", plan_prefix.replace('%', "\\%").replace('_', "\\_"));
        let mut statement = self.conn.prepare(
            "SELECT keys.task_key, task_states.state FROM (
                 SELECT task_key FROM task_states
                 UNION SELECT task_key FROM jobs
                 UNION SELECT task_key FROM reviews
                 UNION SELECT task_key FROM review_drafts
                 UNION SELECT task_key FROM review_resolutions
             ) AS keys
             LEFT JOIN task_states ON task_states.task_key = keys.task_key
             WHERE keys.task_key LIKE ?1 ESCAPE '\\' ORDER BY keys.task_key",
        )?;
        let rows = statement
            .query_map(params![pattern], |row| {
                let key: String = row.get(0)?;
                let state = row
                    .get::<_, Option<String>>(1)?
                    .map(parse_task_state)
                    .transpose()?;
                Ok((key, state))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Re-key a task's durable history: lifecycle state, jobs, reviews,
    /// review drafts, and review resolutions all move from `old_key` to
    /// `new_key` in one transaction. Refuses when the target already has
    /// history in any of those tables (two histories must never merge
    /// silently). Events are append-only history and keep the ids that
    /// were true when they were recorded. Task *nodes* are derived and
    /// re-keyed by the next `index_plan`.
    pub fn migrate_task_key(&mut self, old_key: &str, new_key: &str) -> Result<(), GraphError> {
        let transaction = self.conn.transaction()?;
        let target_exists: bool = transaction
            .query_row(
                "SELECT 1 FROM (
                     SELECT task_key FROM task_states
                     UNION SELECT task_key FROM jobs
                     UNION SELECT task_key FROM reviews
                     UNION SELECT task_key FROM review_drafts
                     UNION SELECT task_key FROM review_resolutions
                 ) WHERE task_key = ?1",
                params![new_key],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if target_exists {
            return Err(GraphError::TaskKeyMigrationConflict {
                old_key: old_key.to_string(),
                new_key: new_key.to_string(),
            });
        }
        for table in [
            "task_states",
            "jobs",
            "reviews",
            "review_drafts",
            "review_resolutions",
        ] {
            transaction.execute(
                &format!("UPDATE {table} SET task_key = ?1 WHERE task_key = ?2"),
                params![new_key, old_key],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn create_node(&mut self, node: &Node) -> Result<NodeId, GraphError> {
        let payload = serde_json::to_string(&node.payload)?;
        self.conn.execute(
            "INSERT INTO nodes (id, kind, name, payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                node.id.0.to_string(),
                node.kind.as_str(),
                node.name,
                payload,
                node.created_at.to_rfc3339()
            ],
        )?;
        self.emit_event(&Event::NodeCreated { node_id: node.id })?;
        Ok(node.id)
    }

    /// Update an existing node's payload by ID.
    pub fn update_node_payload(&mut self, id: NodeId, payload: Value) -> Result<(), GraphError> {
        let payload = serde_json::to_string(&payload)?;
        self.conn.execute(
            "UPDATE nodes SET payload = ?1, created_at = ?2 WHERE id = ?3",
            params![payload, Utc::now().to_rfc3339(), id.0.to_string()],
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
            let payload = serde_json::to_string(&node.payload)?;
            self.conn.execute(
                "UPDATE nodes SET payload = ?1, created_at = ?2 WHERE id = ?3",
                params![payload, Utc::now().to_rfc3339(), id.0.to_string()],
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
            let payload = serde_json::to_string(&payload)?;
            self.conn.execute(
                "UPDATE nodes SET payload = ?1, created_at = ?2 WHERE id = ?3",
                params![payload, Utc::now().to_rfc3339(), id.0.to_string()],
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
            Ok((edge_kind_str, node))
        })?;
        rows.map(|row| {
            let (edge_kind_str, node) = row?;
            let edge_kind = match edge_kind_str.as_str() {
                "depends_on" => EdgeKind::DependsOn,
                "implements" => EdgeKind::Implements,
                "tests" => EdgeKind::Tests,
                "reviews" => EdgeKind::Reviews,
                "deploys" => EdgeKind::Deploys,
                "learns_from" => EdgeKind::LearnsFrom,
                "contains" => EdgeKind::Contains,
                // A row written by a newer binary is an error to route,
                // not a reason to crash the reader.
                _ => {
                    return Err(GraphError::UnknownEdgeKind {
                        kind: edge_kind_str,
                    });
                }
            };
            Ok((edge_kind, node))
        })
        .collect()
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
        if let Some(embedding) = embedding {
            Self::require_finite_embedding(embedding, "model output")?;
        }
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
            let payload = serde_json::to_string(&payload)?;
            tx.execute(
                "UPDATE nodes SET payload = ?1, created_at = ?2 WHERE id = ?3",
                params![payload, Utc::now().to_rfc3339(), id.0.to_string()],
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
            let payload = serde_json::to_string(&node.payload)?;
            tx.execute(
                "INSERT INTO nodes (id, kind, name, payload, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.0.to_string(),
                    node.kind.as_str(),
                    node.name,
                    payload,
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
            let emb_json = serde_json::to_string(emb)?;
            tx.execute(
                "INSERT OR REPLACE INTO code_embeddings (node_id, embedding) VALUES (?1, ?2)",
                params![id.0.to_string(), emb_json],
            )?;
        } else {
            tx.execute(
                "DELETE FROM code_embeddings WHERE node_id = ?1",
                params![id.0.to_string()],
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
        Self::require_finite_embedding(query_embedding, "query")?;
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

        let vectors = rows.collect::<Result<Vec<_>, _>>()?;
        let mut scored = Vec::new();
        for (node, embedding) in vectors {
            Self::require_finite_embedding(
                &embedding,
                &format!("stored vector for {}", node.name),
            )?;
            if let Some(score) = crate::embed::cosine_similarity(query_embedding, &embedding) {
                scored.push((node, score));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    fn require_finite_embedding(embedding: &[f32], context: &str) -> Result<(), GraphError> {
        if embedding.is_empty() {
            return Err(GraphError::InvalidEmbedding(format!(
                "{context} is an empty vector"
            )));
        }
        if let Some(index) = embedding.iter().position(|value| !value.is_finite()) {
            return Err(GraphError::InvalidEmbedding(format!(
                "{context} contains a non-finite value at index {index}"
            )));
        }
        Ok(())
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
            "removed": false,
        });
        let plan_node = Node::new(NodeKind::Plan, relative_path, plan_payload);
        let plan_id = self.upsert_node_by_name(&plan_node)?;

        // Stable task keys are durable graph identities. Keep current task
        // nodes and update them in place; tasks removed from the plan become
        // disconnected tombstones. Rebuilding or deleting every task UUID
        // made append-only lifecycle events unjoinable after reindex.
        let prefix = format!("{}#%", relative_path);
        let current_names: std::collections::HashSet<String> = plan
            .tasks
            .iter()
            .map(|task| format!("{}#{}", relative_path, task.id))
            .collect();
        let existing_tasks: Vec<(String, String, String)> = self
            .conn
            .prepare("SELECT id, name, payload FROM nodes WHERE kind = 'task' AND name LIKE ?1")?
            .query_map(params![&prefix], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let stale_ids: Vec<String> = existing_tasks
            .iter()
            .filter(|(_, name, _)| !current_names.contains(name))
            .map(|(id, _, _)| id.clone())
            .collect();
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
            for (id, name, raw_payload) in &existing_tasks {
                if current_names.contains(name) {
                    continue;
                }
                let mut payload: Value = serde_json::from_str(raw_payload)?;
                if let Some(object) = payload.as_object_mut() {
                    object.insert("removed".to_string(), Value::Bool(true));
                } else {
                    payload = serde_json::json!({ "removed": true });
                }
                let id = NodeId(Uuid::parse_str(id).map_err(|error| {
                    GraphError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    ))
                })?);
                self.update_node_payload(id, payload)?;
            }
        }

        // Contains and inferred/file dependency edges are derived from the
        // current plan contents. Refresh those edges without replacing the
        // task nodes they connect.
        self.conn.execute(
            "DELETE FROM edges WHERE from_node = ?1 AND kind = 'contains'",
            params![plan_id.0.to_string()],
        )?;
        for (id, name, _) in &existing_tasks {
            if current_names.contains(name) {
                self.conn.execute(
                    "DELETE FROM edges WHERE from_node = ?1 AND kind = 'depends_on'",
                    params![id],
                )?;
            }
        }

        for task in &plan.tasks {
            let task_name = format!("{}#{}", relative_path, task.id);
            let task_payload = serde_json::json!({
                "description": task.description,
                "done": task.done,
                "run": task.run,
                "stop": task.stop,
                "removed": false,
            });
            let task_node = Node::new(NodeKind::Task, &task_name, task_payload);
            let task_id = self.upsert_node_by_name(&task_node)?;

            self.create_edge(plan_id, task_id, EdgeKind::Contains)?;
            self.record_event(&Event::TaskPlanned {
                task_id,
                plan_id,
                task_key: task_name.clone(),
                plan_path: relative_path.to_string(),
            })?;

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

    fn graph_with_review_job(task_key: &str, idempotency_key: &str) -> (Graph, Job) {
        let mut graph = Graph::open_in_memory().unwrap();
        graph
            .initialize_task_state(task_key, TaskState::Ready)
            .unwrap();
        graph.transition_task(task_key, TaskState::Running).unwrap();
        let job = graph.create_job(task_key, idempotency_key).unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();
        graph
            .record_job_result(
                &JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap(),
            )
            .unwrap();
        graph.transition_task(task_key, TaskState::Review).unwrap();
        (graph, job)
    }

    fn create_events_table_for_claimed_v1_schema(connection: &Connection) {
        connection
            .execute_batch(
                "CREATE TABLE events (
                    id TEXT PRIMARY KEY,
                    kind TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );",
            )
            .unwrap();
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
    fn schema_indexes_event_history_and_uses_task_state_primary_key() {
        let graph = Graph::open_in_memory().unwrap();
        let event_plan: String = graph
            .conn
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT payload, created_at FROM events
                 ORDER BY created_at DESC LIMIT 20",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            event_plan.contains("idx_events_created_at"),
            "event history must avoid a full scan and temporary sort: {event_plan}"
        );

        let task_plan: String = graph
            .conn
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT state FROM task_states WHERE task_key = 'plans/f.plan.md#task'",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            task_plan.contains("sqlite_autoindex_task_states_1"),
            "the task_key primary key must remain the lifecycle lookup index: {task_plan}"
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
        create_events_table_for_claimed_v1_schema(&conn);
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
        create_events_table_for_claimed_v1_schema(&conn);
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
        create_events_table_for_claimed_v1_schema(&conn);
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
        create_events_table_for_claimed_v1_schema(&conn);
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
        create_events_table_for_claimed_v1_schema(&conn);
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
    fn non_finite_and_corrupt_embeddings_fail_closed() {
        let mut graph = Graph::open_in_memory().unwrap();
        assert!(
            graph
                .index_code_with_embedding("src/bad.rs", "fn bad() {}", Some(&[f32::NAN]))
                .is_err(),
            "non-finite model output must be rejected before JSON turns it into null"
        );
        assert!(
            graph
                .find_node_by_name(NodeKind::Code, "src/bad.rs")
                .unwrap()
                .is_none(),
            "embedding validation must happen before graph mutation"
        );

        let id = graph
            .index_code_with_embedding("src/good.rs", "fn good() {}", Some(&[1.0, 0.0]))
            .unwrap();
        graph
            .conn
            .execute(
                "UPDATE code_embeddings SET embedding = '[null]' WHERE node_id = ?1",
                params![id.0.to_string()],
            )
            .unwrap();
        assert!(
            graph.semantic_search(&[1.0, 0.0], 5).is_err(),
            "malformed stored vectors must surface instead of disappearing from results"
        );
        assert!(graph.semantic_search(&[f32::INFINITY], 5).is_err());
    }

    #[test]
    fn reindexing_changed_code_without_a_vector_invalidates_the_old_embedding() {
        let mut graph = Graph::open_in_memory().unwrap();
        graph
            .index_code_with_embedding("src/changing.rs", "fn old() {}", Some(&[1.0, 0.0]))
            .unwrap();
        assert_eq!(graph.semantic_search(&[1.0, 0.0], 5).unwrap().len(), 1);

        graph
            .index_code("src/changing.rs", "fn replacement() {}")
            .unwrap();

        assert!(
            graph.semantic_search(&[1.0, 0.0], 5).unwrap().is_empty(),
            "semantic search must never score a vector produced for old content"
        );
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
    fn reindexing_a_plan_preserves_durable_task_node_identity() {
        let mut graph = Graph::open_in_memory().unwrap();
        let plan = crate::plan::Plan::parse(
            "features",
            "# Features\n\n1. [ ] Keep identity - id: keep-identity\n",
        );
        let task_key = "plans/features.plan.md#keep-identity";

        graph.index_plan("plans/features.plan.md", &plan).unwrap();
        let first = graph
            .find_node_by_name(NodeKind::Task, task_key)
            .unwrap()
            .unwrap()
            .id;

        graph.index_plan("plans/features.plan.md", &plan).unwrap();
        let second = graph
            .find_node_by_name(NodeKind::Task, task_key)
            .unwrap()
            .unwrap()
            .id;

        assert_eq!(
            first, second,
            "a stable task key must keep the same graph identity across reindex"
        );
        let task_events = graph
            .events(100)
            .unwrap()
            .into_iter()
            .filter_map(|(_, event)| match event {
                Event::TaskPlanned { task_key, .. } => Some(task_key),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(task_events, vec![task_key, task_key]);
        let integrity = graph.event_reference_integrity().unwrap();
        assert!(integrity.events_scanned > 0);
        assert_eq!(
            integrity.events_with_missing_references, 0,
            "ordinary reindex must not orphan append-only event references"
        );
        assert_eq!(integrity.missing_node_references, 0);
        assert_eq!(integrity.unresolvable_events, 0);
    }

    #[test]
    fn removing_a_planned_task_tombstones_its_node_for_event_history() {
        let mut graph = Graph::open_in_memory().unwrap();
        let original = crate::plan::Plan::parse(
            "features",
            "# Features\n\n1. [ ] Keep - id: keep\n2. [ ] Remove - id: remove\n",
        );
        graph
            .index_plan("plans/features.plan.md", &original)
            .unwrap();
        let removed_key = "plans/features.plan.md#remove";
        let removed_id = graph
            .find_node_by_name(NodeKind::Task, removed_key)
            .unwrap()
            .unwrap()
            .id;

        let edited = crate::plan::Plan::parse("features", "# Features\n\n1. [ ] Keep - id: keep\n");
        graph.index_plan("plans/features.plan.md", &edited).unwrap();

        let tombstone = graph
            .find_node_by_name(NodeKind::Task, removed_key)
            .unwrap()
            .expect("removed task keeps a historical join target");
        assert_eq!(tombstone.id, removed_id);
        assert_eq!(tombstone.payload["removed"], true);
        assert_eq!(
            graph
                .event_reference_integrity()
                .unwrap()
                .missing_node_references,
            0
        );
    }

    #[test]
    fn rebuild_preserves_plan_and_task_identities_as_tombstones() {
        let mut graph = Graph::open_in_memory().unwrap();
        let plan = crate::plan::Plan::parse("features", "# F\n\n1. [ ] Stable - id: stable\n");
        graph.index_plan("plans/f.plan.md", &plan).unwrap();
        let plan_id = graph
            .find_node_by_name(NodeKind::Plan, "plans/f.plan.md")
            .unwrap()
            .unwrap()
            .id;
        let task_id = graph
            .find_node_by_name(NodeKind::Task, "plans/f.plan.md#stable")
            .unwrap()
            .unwrap()
            .id;

        graph.truncate_derived().unwrap();
        assert_eq!(
            graph.get_node(plan_id).unwrap().unwrap().payload["removed"],
            true
        );
        assert_eq!(
            graph.get_node(task_id).unwrap().unwrap().payload["removed"],
            true
        );

        graph.index_plan("plans/f.plan.md", &plan).unwrap();
        assert_eq!(
            graph
                .find_node_by_name(NodeKind::Plan, "plans/f.plan.md")
                .unwrap()
                .unwrap()
                .id,
            plan_id
        );
        assert_eq!(
            graph
                .find_node_by_name(NodeKind::Task, "plans/f.plan.md#stable")
                .unwrap()
                .unwrap()
                .id,
            task_id
        );
        assert_eq!(
            graph
                .event_reference_integrity()
                .unwrap()
                .missing_node_references,
            0
        );
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
    fn job_claim_and_finish_commit_lifecycle_as_one_idempotent_protocol() {
        let mut graph = Graph::open_in_memory().unwrap();
        let (job, claimed) = graph.claim_job("task-claim", "claim-1").unwrap();
        assert!(claimed);
        assert_eq!(job.state, JobState::Running);
        assert_eq!(
            graph.task_state("task-claim").unwrap(),
            Some(TaskState::Running)
        );
        let result = JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap();

        assert_eq!(
            graph.finish_job("task-claim", &result).unwrap(),
            TaskState::Review
        );
        let (replayed, claimed) = graph.claim_job("task-claim", "claim-1").unwrap();
        assert!(!claimed);
        assert_eq!(replayed.id, job.id);
        assert_eq!(replayed.state, JobState::Succeeded);
        assert_eq!(graph.job_result(job.id).unwrap(), Some(result));
    }

    #[test]
    fn failed_terminal_task_transition_rolls_back_job_and_result_together() {
        let mut graph = Graph::open_in_memory().unwrap();
        let (job, claimed) = graph
            .claim_job("task-finish-atomic", "finish-atomic-1")
            .unwrap();
        assert!(claimed);
        graph
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_review_transition
                 BEFORE UPDATE ON task_states
                 WHEN NEW.state = 'review'
                 BEGIN
                     SELECT RAISE(ABORT, 'forced task transition failure');
                 END;",
            )
            .unwrap();
        let result = JobResult::new(job.id, JobState::Succeeded, evidence_governance()).unwrap();

        assert!(graph.finish_job("task-finish-atomic", &result).is_err());
        assert_eq!(
            graph.task_state("task-finish-atomic").unwrap(),
            Some(TaskState::Running)
        );
        assert_eq!(
            graph
                .job_by_idempotency_key("finish-atomic-1")
                .unwrap()
                .unwrap()
                .state,
            JobState::Running
        );
        assert!(graph.job_result(job.id).unwrap().is_none());
    }

    #[test]
    fn job_persistence_rejects_a_reused_key_from_another_task() {
        let mut graph = Graph::open_in_memory().unwrap();
        let original = graph.create_job("task-a", "same-operation").unwrap();
        let repeated = graph.create_job("task-b", "same-operation");

        assert!(matches!(
            repeated,
            Err(GraphError::IdempotencyKeyConflict {
                existing_task,
                requested_task,
                ..
            }) if existing_task == "task-a" && requested_task == "task-b"
        ));
        assert_eq!(
            graph
                .job_by_idempotency_key("same-operation")
                .unwrap()
                .unwrap(),
            original
        );
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
        assert!(matches!(
            graph.record_job_result(&result),
            Err(GraphError::JobResultConflict { job_id }) if job_id == job.id
        ));
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
    fn verify_migration_checksums_detects_tampered_and_unknown_rows() {
        let graph = Graph::open_in_memory().unwrap();
        let clean = graph.verify_migration_checksums().unwrap();
        assert_eq!(clean.len() as i64, LATEST_SCHEMA_VERSION);
        assert!(
            clean
                .iter()
                .all(|report| report.status == MigrationChecksumStatus::Verified)
        );

        graph
            .conn
            .execute(
                "UPDATE schema_migrations SET checksum = ?1 WHERE version = 2",
                params!["0".repeat(64)],
            )
            .unwrap();
        graph
            .conn
            .execute(
                "INSERT INTO schema_migrations (version, checksum, applied_at)
                 VALUES (9999, ?1, '2026-01-01T00:00:00Z')",
                params!["1".repeat(64)],
            )
            .unwrap();

        let audited = graph.verify_migration_checksums().unwrap();
        let tampered = audited.iter().find(|report| report.version == 2).unwrap();
        assert!(matches!(
            tampered.status,
            MigrationChecksumStatus::Mismatch { .. }
        ));
        let unknown = audited
            .iter()
            .find(|report| report.version == 9999)
            .unwrap();
        assert_eq!(unknown.status, MigrationChecksumStatus::UnknownVersion);
        assert!(
            audited
                .iter()
                .filter(|report| report.version != 2 && report.version != 9999)
                .all(|report| report.status == MigrationChecksumStatus::Verified)
        );
    }

    #[test]
    fn lost_transition_races_surface_instead_of_double_claiming() {
        let mut graph = Graph::open_in_memory().unwrap();
        graph
            .initialize_task_state("task-a", TaskState::Ready)
            .unwrap();
        // Simulate the loser of a read-validate-write race: the row moved
        // to Running after this operator's validation read said Ready.
        // Ready -> Running is a legal edge, but the conditional update must
        // refuse rather than overwrite the winner's claim.
        graph.transition_task("task-a", TaskState::Running).unwrap();
        let err = graph
            .transition_task_from("task-a", TaskState::Ready, TaskState::Running)
            .expect_err("lost race must surface");
        assert!(matches!(err, GraphError::StaleTransition { .. }), "{err}");

        let job = graph.create_job("task-a", "race-1").unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        let err = graph
            .transition_job_from(job.id, JobState::Queued, JobState::Running)
            .expect_err("lost job race must surface");
        assert!(matches!(err, GraphError::StaleTransition { .. }), "{err}");
    }

    #[test]
    fn corrupt_stored_events_error_instead_of_panicking() {
        let mut graph = Graph::open_in_memory().unwrap();
        graph
            .record_event(&Event::SnapshotCreated {
                name: "clean".into(),
                path: "s.sqlite".into(),
            })
            .unwrap();
        assert_eq!(graph.events(10).unwrap().len(), 1);

        // Undecodable payload: an error to route, never a crash.
        graph
            .conn
            .execute(
                "INSERT INTO events (id, kind, payload, created_at)
                 VALUES (?1, 'broken', '{\"event\": tru', ?2)",
                params![Uuid::new_v4().to_string(), Utc::now().to_rfc3339()],
            )
            .unwrap();
        let err = graph.events(10).expect_err("corrupt row must surface");
        assert!(
            matches!(err, GraphError::CorruptStoredEvent { .. }),
            "{err}"
        );

        // An event kind from a newer binary fails the same recoverable way.
        graph
            .conn
            .execute(
                "UPDATE events SET payload = '{\"event\":\"from_the_future\"}'
                 WHERE kind = 'broken'",
                [],
            )
            .unwrap();
        let err = graph.events(10).expect_err("unknown kind must surface");
        assert!(
            matches!(err, GraphError::CorruptStoredEvent { .. }),
            "{err}"
        );

        // A bad timestamp too.
        graph
            .conn
            .execute(
                "UPDATE events SET payload = '{\"event\":\"node_created\",\"node_id\":\"00000000-0000-0000-0000-000000000000\"}',
                 created_at = 'not-a-time' WHERE kind = 'broken'",
                [],
            )
            .unwrap();
        let err = graph.events(10).expect_err("bad timestamp must surface");
        assert!(
            matches!(err, GraphError::CorruptStoredEvent { .. }),
            "{err}"
        );
    }

    #[test]
    fn review_binding_refuses_mismatched_or_unreviewable_pairs() {
        let mut graph = Graph::open_in_memory().unwrap();
        graph
            .initialize_task_state("task-a", TaskState::Ready)
            .unwrap();
        graph.transition_task("task-a", TaskState::Running).unwrap();
        let job = graph.create_job("task-a", "binding-1").unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();

        // Task not yet in review: the decision is not actionable.
        assert!(graph.validate_review_binding("task-a", job.id).is_err());
        graph.transition_task("task-a", TaskState::Review).unwrap();
        assert!(graph.validate_review_binding("task-a", job.id).is_ok());

        // A job belonging to another task must never authorize promotion,
        // whatever state the named task is in.
        graph
            .initialize_task_state("task-b", TaskState::Ready)
            .unwrap();
        graph.transition_task("task-b", TaskState::Running).unwrap();
        graph.transition_task("task-b", TaskState::Review).unwrap();
        assert!(graph.validate_review_binding("task-b", job.id).is_err());
        // Unknown job: refused.
        assert!(
            graph
                .validate_review_binding("task-a", JobId(Uuid::new_v4()))
                .is_err()
        );
    }

    #[test]
    fn migrate_task_key_moves_all_durable_history_and_refuses_conflicts() {
        let mut graph = Graph::open_in_memory().unwrap();
        let old_key = "plans/f.plan.md#task-3";
        let new_key = "plans/f.plan.md#cap-runner-output";
        graph
            .initialize_task_state(old_key, TaskState::Ready)
            .unwrap();
        let job = graph.create_job(old_key, "migrate-1").unwrap();
        let job_id = job.id.0.to_string();
        for (table, sql) in [
            (
                "reviews",
                "INSERT INTO reviews (id, task_key, job_id, decision, reviewer, reason, created_at)
                 VALUES ('r1', ?1, ?2, 'approve', 'megloff1', 'ok', '2026-07-17')",
            ),
            (
                "review_drafts",
                "INSERT INTO review_drafts
                     (id, task_key, job_id, perspective, recommendation, body, agent, created_at)
                 VALUES ('d1', ?1, ?2, 'evidence', 'approve', 'body', 'm', '2026-07-17')",
            ),
            (
                "review_resolutions",
                "INSERT INTO review_resolutions
                     (id, task_key, job_id, selected_draft_id, edit_similarity,
                      final_body, decision, reviewer, created_at)
                 VALUES ('res1', ?1, ?2, NULL, 0.5, 'final', 'approve', 'megloff1', '2026-07-17')",
            ),
        ] {
            graph
                .conn
                .execute(sql, params![old_key, job_id])
                .unwrap_or_else(|e| panic!("seeding {table}: {e}"));
        }

        graph.migrate_task_key(old_key, new_key).unwrap();

        assert_eq!(
            graph.task_state(new_key).unwrap(),
            Some(TaskState::Ready),
            "lifecycle state moved"
        );
        assert_eq!(graph.task_state(old_key).unwrap(), None);
        for table in ["jobs", "reviews", "review_drafts", "review_resolutions"] {
            let count: i64 = graph
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE task_key = ?1"),
                    params![new_key],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "{table} must reference the new key");
            let stale: i64 = graph
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE task_key = ?1"),
                    params![old_key],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(stale, 0, "{table} must not reference the old key");
        }
        assert_eq!(
            graph
                .job_by_idempotency_key("migrate-1")
                .unwrap()
                .unwrap()
                .task_key,
            new_key
        );
        let _ = job;

        // A second history under the old key must never merge into the
        // migrated one.
        graph
            .initialize_task_state(old_key, TaskState::Ready)
            .unwrap();
        match graph.migrate_task_key(old_key, new_key) {
            Err(GraphError::TaskKeyMigrationConflict { .. }) => {}
            other => panic!("expected TaskKeyMigrationConflict, got {other:?}"),
        }

        // Listing by prefix sees both, escaped LIKE and all.
        let keys = graph.task_keys_with_state("plans/f.plan.md#").unwrap();
        assert_eq!(
            keys.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec![new_key, old_key],
        );
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
    fn staged_review_blocks_an_opposite_decision_and_same_decision_resumes() {
        let (mut graph, job) = graph_with_review_job("task-pending", "pending-review-1");
        let approval = ReviewResolution {
            id: Uuid::new_v4(),
            task_key: "task-pending".into(),
            job_id: job.id,
            selected_draft_id: None,
            original_draft: None,
            final_body: "The immutable evidence supports promotion".into(),
            edit_similarity: None,
            decision: ReviewDecision::Approve,
            reviewer: "human@example.test".into(),
            created_at: Utc::now(),
        };

        let staged = graph.stage_review_resolution(&approval).unwrap();
        assert_eq!(
            graph.task_state("task-pending").unwrap(),
            Some(TaskState::Review)
        );
        let same_decision_retry = ReviewResolution {
            id: Uuid::new_v4(),
            ..approval.clone()
        };
        assert_eq!(
            graph
                .stage_review_resolution(&same_decision_retry)
                .unwrap()
                .id,
            staged.id,
            "a retry resumes the durable resolution instead of replacing it"
        );
        let opposite = ReviewResolution {
            id: Uuid::new_v4(),
            decision: ReviewDecision::Reject,
            ..approval
        };
        assert!(matches!(
            graph.stage_review_resolution(&opposite),
            Err(GraphError::PendingReviewResolution { .. })
        ));

        assert_eq!(
            graph.finalize_staged_review_resolution(staged.id).unwrap(),
            TaskState::Done
        );
        assert_eq!(graph.reviews_for_task("task-pending").unwrap().len(), 1);
        let pending: i64 = graph
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pending_review_resolutions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0);
    }

    #[test]
    fn failed_resolution_finalization_rolls_back_and_keeps_pending_recovery() {
        let (mut graph, job) = graph_with_review_job("task-atomic", "atomic-review-1");
        let resolution = ReviewResolution {
            id: Uuid::new_v4(),
            task_key: "task-atomic".into(),
            job_id: job.id,
            selected_draft_id: None,
            original_draft: None,
            final_body: "Approval should commit as one invariant".into(),
            edit_similarity: None,
            decision: ReviewDecision::Approve,
            reviewer: "human@example.test".into(),
            created_at: Utc::now(),
        };
        let staged = graph.stage_review_resolution(&resolution).unwrap();
        graph
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_review_resolution
                 BEFORE INSERT ON review_resolutions
                 BEGIN
                     SELECT RAISE(ABORT, 'forced resolution failure');
                 END;",
            )
            .unwrap();

        assert!(graph.finalize_staged_review_resolution(staged.id).is_err());
        assert_eq!(
            graph.task_state("task-atomic").unwrap(),
            Some(TaskState::Review)
        );
        assert!(graph.reviews_for_task("task-atomic").unwrap().is_empty());
        let pending: i64 = graph
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pending_review_resolutions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1, "the durable recovery intent must survive");

        graph
            .conn
            .execute_batch("DROP TRIGGER fail_review_resolution")
            .unwrap();
        assert_eq!(
            graph.finalize_staged_review_resolution(staged.id).unwrap(),
            TaskState::Done
        );
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
                task_key: "plans/test.plan.md#do-work".to_string(),
                description: "do work".to_string(),
            })
            .unwrap();
        graph
            .record_event(&Event::TaskCompleted {
                task_id,
                task_key: "plans/test.plan.md#do-work".to_string(),
                description: "do work".to_string(),
            })
            .unwrap();

        let events = graph.events(2).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1.kind(), "task_completed");
        assert_eq!(events[1].1.kind(), "task_started");
    }
}
