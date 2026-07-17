use super::*;

impl Graph {
    pub fn record_review(&mut self, review: &Review) -> Result<TaskState, GraphError> {
        self.in_immediate_transaction(|graph| graph.record_review_inner(review))
    }

    fn record_review_inner(&mut self, review: &Review) -> Result<TaskState, GraphError> {
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
        self.in_immediate_transaction(|graph| graph.record_review_draft_inner(draft))
    }

    fn record_review_draft_inner(&mut self, draft: &ReviewDraft) -> Result<(), GraphError> {
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
            let question_id = self.ensure_discourse_question_inner(
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
            self.record_discourse_turn_inner(&turn)?;
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
    /// A review decision is actionable only when the job belongs to the
    /// named task and that task is awaiting review. Callers must enforce
    /// this before any side effect of the decision — promoting staged bytes
    /// on the strength of an unvalidated task/job pair is a gate bypass.
    pub fn validate_review_binding(&self, task_key: &str, job_id: JobId) -> Result<(), GraphError> {
        let job_task: Option<String> = self
            .conn
            .query_row(
                "SELECT task_key FROM jobs WHERE id = ?1",
                params![job_id.0.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match job_task {
            Some(owner) if owner == task_key => {}
            _ => return Err(GraphError::ResultStateMismatch),
        }
        if self.task_state(task_key)? != Some(TaskState::Review) {
            return Err(GraphError::ResultStateMismatch);
        }
        Ok(())
    }

    /// Durably reserve one human decision before any cross-store side effect.
    ///
    /// A retry for the same task/job pair returns the original pending
    /// resolution, so a crash after workspace promotion cannot be followed by
    /// an opposite decision. The caller must resume that resolution and then
    /// finalize it.
    pub fn stage_review_resolution(
        &mut self,
        proposed: &ReviewResolution,
    ) -> Result<ReviewResolution, GraphError> {
        self.in_immediate_transaction(|graph| {
            let existing: Option<String> = graph
                .conn
                .query_row(
                    "SELECT resolution FROM pending_review_resolutions
                     WHERE task_key = ?1 AND job_id = ?2",
                    params![proposed.task_key, proposed.job_id.0.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(existing) = existing {
                let existing: ReviewResolution = serde_json::from_str(&existing)?;
                if existing.decision != proposed.decision {
                    return Err(GraphError::PendingReviewResolution {
                        task_key: proposed.task_key.clone(),
                        job_id: proposed.job_id,
                    });
                }
                tracing::info!(
                    task_key = %existing.task_key,
                    job_id = %existing.job_id.0,
                    resolution_id = %existing.id,
                    "resuming pending review resolution"
                );
                return Ok(existing);
            }

            graph.validate_initial_review_resolution(proposed)?;
            graph.conn.execute(
                "INSERT INTO pending_review_resolutions
                 (id, task_key, job_id, resolution, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    proposed.id.to_string(),
                    proposed.task_key,
                    proposed.job_id.0.to_string(),
                    serde_json::to_string(proposed)?,
                    proposed.created_at.to_rfc3339(),
                ],
            )?;
            tracing::info!(
                task_key = %proposed.task_key,
                job_id = %proposed.job_id.0,
                resolution_id = %proposed.id,
                decision = ?proposed.decision,
                "staged review resolution"
            );
            Ok(proposed.clone())
        })
    }

    /// Atomically record and clear a previously staged human resolution.
    pub fn finalize_staged_review_resolution(
        &mut self,
        resolution_id: Uuid,
    ) -> Result<TaskState, GraphError> {
        self.in_immediate_transaction(|graph| {
            let raw: String = graph.conn.query_row(
                "SELECT resolution FROM pending_review_resolutions WHERE id = ?1",
                params![resolution_id.to_string()],
                |row| row.get(0),
            )?;
            let resolution: ReviewResolution = serde_json::from_str(&raw)?;
            let state = graph.record_review_resolution_inner(&resolution)?;
            let deleted = graph.conn.execute(
                "DELETE FROM pending_review_resolutions WHERE id = ?1",
                params![resolution_id.to_string()],
            )?;
            if deleted != 1 {
                return Err(GraphError::ResultStateMismatch);
            }
            tracing::info!(
                task_key = %resolution.task_key,
                job_id = %resolution.job_id.0,
                resolution_id = %resolution.id,
                state = ?state,
                "finalized review resolution"
            );
            Ok(state)
        })
    }

    pub fn record_review_resolution(
        &mut self,
        resolution: &ReviewResolution,
    ) -> Result<TaskState, GraphError> {
        self.in_immediate_transaction(|graph| {
            let pending: bool = graph.conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pending_review_resolutions
                    WHERE task_key = ?1 AND job_id = ?2
                )",
                params![resolution.task_key, resolution.job_id.0.to_string()],
                |row| row.get(0),
            )?;
            if pending {
                return Err(GraphError::PendingReviewResolution {
                    task_key: resolution.task_key.clone(),
                    job_id: resolution.job_id,
                });
            }
            graph.record_review_resolution_inner(resolution)
        })
    }

    fn validate_initial_review_resolution(
        &self,
        resolution: &ReviewResolution,
    ) -> Result<(), GraphError> {
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
        self.validate_review_binding(&resolution.task_key, resolution.job_id)?;
        let job_state: String = self.conn.query_row(
            "SELECT state FROM jobs WHERE id = ?1",
            params![resolution.job_id.0.to_string()],
            |row| row.get(0),
        )?;
        if parse_job_state_value(&job_state)? != JobState::Succeeded
            || self.job_result(resolution.job_id)?.is_none()
        {
            return Err(GraphError::ResultStateMismatch);
        }
        Ok(())
    }

    fn record_review_resolution_inner(
        &mut self,
        resolution: &ReviewResolution,
    ) -> Result<TaskState, GraphError> {
        if self.task_state(&resolution.task_key)? == Some(TaskState::Review) {
            self.validate_initial_review_resolution(resolution)?;
        } else if let Some(draft_id) = resolution.selected_draft_id {
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
            self.record_review_inner(&review)?
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
        let question_id = self.ensure_discourse_question_inner(
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
                self.record_discourse_turn_inner(&DiscourseTurn {
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
        self.record_discourse_turn_inner(&DiscourseTurn {
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
}
