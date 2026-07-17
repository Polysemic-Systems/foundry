use super::*;

impl Graph {
    /// Atomically claim a fresh job and its task, or return an existing
    /// same-task job for idempotent replay without changing lifecycle state.
    pub fn claim_job(
        &mut self,
        task_key: &str,
        idempotency_key: &str,
    ) -> Result<(Job, bool), GraphError> {
        self.in_immediate_transaction(|graph| {
            if let Some(job) = graph.job_by_idempotency_key(idempotency_key)? {
                if job.task_key != task_key {
                    return Err(GraphError::IdempotencyKeyConflict {
                        key: idempotency_key.to_string(),
                        existing_task: job.task_key,
                        requested_task: task_key.to_string(),
                    });
                }
                tracing::info!(
                    task_key,
                    job_id = %job.id.0,
                    idempotency_key,
                    state = ?job.state,
                    "replaying existing job"
                );
                return Ok((job, false));
            }

            let mut task_state = graph.initialize_task_state(task_key, TaskState::Ready)?;
            if task_state == TaskState::Failed {
                task_state = graph.transition_task(task_key, TaskState::Ready)?;
            }
            if task_state != TaskState::Ready {
                return Err(GraphError::TaskStateMismatch {
                    task_key: task_key.to_string(),
                    current: task_state,
                    expected: TaskState::Ready,
                });
            }
            let mut job = graph.create_job(task_key, idempotency_key)?;
            graph.transition_task(task_key, TaskState::Running)?;
            job.state = graph.transition_job(job.id, JobState::Running)?;
            tracing::info!(
                task_key,
                job_id = %job.id.0,
                idempotency_key,
                "claimed job"
            );
            Ok((job, true))
        })
    }

    /// Atomically persist one terminal result and the corresponding job/task
    /// transitions. A filesystem-backed evidence object written before a SQL
    /// rollback is an unreferenced blob and remains eligible for orphan GC.
    pub fn finish_job(
        &mut self,
        task_key: &str,
        result: &JobResult,
    ) -> Result<TaskState, GraphError> {
        self.in_immediate_transaction(|graph| {
            let (job_task, job_state): (String, String) = graph.conn.query_row(
                "SELECT task_key, state FROM jobs WHERE id = ?1",
                params![result.job_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if job_task != task_key || parse_job_state_value(&job_state)? != JobState::Running {
                return Err(GraphError::ResultStateMismatch);
            }
            let task_state = graph
                .task_state(task_key)?
                .ok_or(GraphError::ResultStateMismatch)?;
            if task_state != TaskState::Running {
                return Err(GraphError::TaskStateMismatch {
                    task_key: task_key.to_string(),
                    current: task_state,
                    expected: TaskState::Running,
                });
            }

            graph.transition_job(result.job_id, result.state)?;
            graph.record_job_result(result)?;
            let state = graph.transition_task(
                task_key,
                if result.state == JobState::Succeeded {
                    TaskState::Review
                } else {
                    TaskState::Failed
                },
            )?;
            tracing::info!(
                task_key,
                job_id = %result.job_id.0,
                job_state = ?result.state,
                task_state = ?state,
                "finished job"
            );
            Ok(state)
        })
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
        self.transition_task_from(task_key, current, next)
    }

    /// The write half of a task transition, conditional on the state the
    /// caller validated against. Optimistic concurrency: the update lands
    /// only if the row still holds that state, so a concurrent operator who
    /// got there first surfaces as a lost race, never a double claim.
    pub(super) fn transition_task_from(
        &mut self,
        task_key: &str,
        current: TaskState,
        next: TaskState,
    ) -> Result<TaskState, GraphError> {
        let next = current.transition(next)?;
        let updated = self.conn.execute(
            "UPDATE task_states SET state = ?1, updated_at = ?2
             WHERE task_key = ?3 AND state = ?4",
            params![
                next.as_str(),
                Utc::now().to_rfc3339(),
                task_key,
                current.as_str()
            ],
        )?;
        if updated == 0 {
            return Err(GraphError::StaleTransition {
                key: task_key.to_string(),
                expected: current.as_str().to_string(),
            });
        }
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
        let job = self
            .job_by_idempotency_key(idempotency_key)?
            .ok_or_else(|| GraphError::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
        if job.task_key != task_key {
            return Err(GraphError::IdempotencyKeyConflict {
                key: idempotency_key.to_string(),
                existing_task: job.task_key,
                requested_task: task_key.to_string(),
            });
        }
        Ok(job)
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
        self.transition_job_from(id, current, next)
    }

    /// Same optimistic guard as `transition_task_from`: no lost update can
    /// silently overwrite a concurrent operator's claim.
    pub(super) fn transition_job_from(
        &mut self,
        id: JobId,
        current: JobState,
        next: JobState,
    ) -> Result<JobState, GraphError> {
        let next = current.transition(next)?;
        let updated = self.conn.execute(
            "UPDATE jobs SET state = ?1, updated_at = ?2 WHERE id = ?3 AND state = ?4",
            params![
                next.as_str(),
                Utc::now().to_rfc3339(),
                id.0.to_string(),
                current.as_str()
            ],
        )?;
        if updated == 0 {
            return Err(GraphError::StaleTransition {
                key: id.0.to_string(),
                expected: current.as_str().to_string(),
            });
        }
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
        if let Some(existing) = self.job_result(result.job_id)? {
            if existing == *result {
                return Ok(());
            }
            return Err(GraphError::JobResultConflict {
                job_id: result.job_id,
            });
        }
        let stored = crate::evidence_store::externalize_job_result(&self.conn, result)?;
        self.conn.execute(
            "INSERT INTO job_results (job_id, result, created_at) VALUES (?1, ?2, ?3)",
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
}
