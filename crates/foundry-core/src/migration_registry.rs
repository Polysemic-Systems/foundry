//! Canonical registry of schema migrations and their SHA-256 checksums.
//!
//! This module is the single source of truth for the SQL that constructs and
//! evolves the graph database schema. Consumers that need to verify or
//! backfill migration checksums must use this registry rather than computing
//! digests from their own copy of the migration SQL.

use sha2::{Digest, Sha256};

/// Returns the known schema migrations as `(version, sql)` pairs in ascending
/// order.
pub fn migrations() -> Vec<(i64, &'static str)> {
    vec![
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
        (
            6,
            "CREATE TABLE IF NOT EXISTS review_drafts (
                    id TEXT PRIMARY KEY,
                    task_key TEXT NOT NULL,
                    job_id TEXT NOT NULL,
                    perspective TEXT NOT NULL,
                    recommendation TEXT NOT NULL,
                    body TEXT NOT NULL,
                    agent TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    UNIQUE(job_id, perspective),
                    FOREIGN KEY (job_id) REFERENCES jobs(id)
                );
                CREATE INDEX IF NOT EXISTS idx_review_drafts_job ON review_drafts(job_id);
                CREATE TABLE IF NOT EXISTS review_resolutions (
                    id TEXT PRIMARY KEY,
                    task_key TEXT NOT NULL,
                    job_id TEXT NOT NULL,
                    selected_draft_id TEXT,
                    original_draft TEXT,
                    final_body TEXT NOT NULL,
                    edit_similarity REAL,
                    decision TEXT NOT NULL,
                    reviewer TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    FOREIGN KEY (job_id) REFERENCES jobs(id),
                    FOREIGN KEY (selected_draft_id) REFERENCES review_drafts(id)
                );
                CREATE INDEX IF NOT EXISTS idx_review_resolutions_task
                    ON review_resolutions(task_key);",
        ),
        (
            7,
            "CREATE TABLE IF NOT EXISTS discourse_turns (
                    id TEXT PRIMARY KEY,
                    context_key TEXT NOT NULL,
                    speaker TEXT NOT NULL,
                    act TEXT NOT NULL,
                    body TEXT NOT NULL,
                    reply_to TEXT,
                    created_at TEXT NOT NULL,
                    FOREIGN KEY (reply_to) REFERENCES discourse_turns(id)
                );
                CREATE INDEX IF NOT EXISTS idx_discourse_turns_context
                    ON discourse_turns(context_key, created_at);",
        ),
    ]
}

/// Returns the expected SHA-256 checksum for a known migration version, or
/// `None` if the version is not part of the canonical registry.
pub fn checksum_for(version: i64) -> Option<String> {
    migrations()
        .iter()
        .find(|(v, _)| *v == version)
        .map(|(_, sql)| format!("{:x}", Sha256::digest(sql.as_bytes())))
}
