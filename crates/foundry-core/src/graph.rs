use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use thiserror::Error;
use uuid::Uuid;

use crate::Event;
use crate::job::{
    Job, JobId, JobResult, JobState, Review, ReviewDecision, StateParseError, TaskState,
    TransitionError,
};
use crate::living::ConformanceError;

pub const LATEST_SCHEMA_VERSION: i64 = 5;

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

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            )",
            [],
        )?;

        let current: i64 = self
            .conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);

        let migrations: Vec<(i64, &str)> = vec![
            (
                1,
                "CREATE TABLE IF NOT EXISTS nodes (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_nodes_kind ON nodes(kind);
            CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name);

            CREATE TABLE IF NOT EXISTS edges (
                id TEXT PRIMARY KEY,
                from_node TEXT NOT NULL,
                to_node TEXT NOT NULL,
                kind TEXT NOT NULL,
                FOREIGN KEY (from_node) REFERENCES nodes(id),
                FOREIGN KEY (to_node) REFERENCES nodes(id)
            );
            CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_node);
            CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_node);

            CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);

            CREATE TABLE IF NOT EXISTS code_index (
                id TEXT PRIMARY KEY,
                node_id TEXT NOT NULL,
                content TEXT NOT NULL,
                FOREIGN KEY (node_id) REFERENCES nodes(id)
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS code_search USING fts5(content);",
            ),
            (
                2,
                "CREATE TABLE IF NOT EXISTS code_embeddings (
                node_id TEXT PRIMARY KEY,
                embedding TEXT NOT NULL,
                FOREIGN KEY (node_id) REFERENCES nodes(id)
            );",
            ),
            (
                3,
                "CREATE TABLE IF NOT EXISTS task_states (
                    task_key TEXT PRIMARY KEY,
                    state TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS jobs (
                    id TEXT PRIMARY KEY,
                    task_key TEXT NOT NULL,
                    idempotency_key TEXT NOT NULL UNIQUE,
                    state TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_jobs_task_key ON jobs(task_key);",
            ),
            (
                4,
                "CREATE TABLE IF NOT EXISTS job_results (
                    job_id TEXT PRIMARY KEY,
                    result TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    FOREIGN KEY (job_id) REFERENCES jobs(id)
                );",
            ),
            (
                5,
                "CREATE TABLE IF NOT EXISTS reviews (
                    id TEXT PRIMARY KEY,
                    task_key TEXT NOT NULL,
                    job_id TEXT NOT NULL,
                    decision TEXT NOT NULL,
                    reviewer TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    FOREIGN KEY (job_id) REFERENCES jobs(id)
                );
                CREATE INDEX IF NOT EXISTS idx_reviews_task_key ON reviews(task_key);",
            ),
        ];

        for (version, sql) in migrations {
            if version > current {
                self.conn.execute_batch(sql)?;
                self.conn.execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, Utc::now().to_rfc3339()],
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
        self.conn.execute(
            "INSERT OR IGNORE INTO job_results (job_id, result, created_at) VALUES (?1, ?2, ?3)",
            params![
                result.job_id.0.to_string(),
                serde_json::to_string(result)?,
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
        json.map(|value| serde_json::from_str(&value).map_err(GraphError::from))
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
            serde_json::from_str(&json).map_err(GraphError::from)
        })
        .collect()
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
