use crate::graph::NodeId;
use crate::job::JobId;
use uuid::Uuid;

/// Domain events are the only way subsystems communicate.
/// Each event is typed, versioned, and stored.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A new node entered the graph.
    NodeCreated { node_id: NodeId },
    /// A new edge entered the graph.
    EdgeCreated {
        from: NodeId,
        to: NodeId,
        edge_kind: String,
    },
    /// A task was planned.
    TaskPlanned { task_id: NodeId, plan_id: NodeId },
    /// A plan markdown file was indexed into the graph.
    PlanIndexed { path: String, plan_id: NodeId },
    /// A code file was indexed.
    CodeIndexed {
        path: String,
        node_id: NodeId,
        lines: usize,
    },
    /// A model was invoked.
    ModelInvoked {
        model: String,
        prompt_tokens: u64,
        cost_usd: f64,
    },
    /// A typed turn was added to a Socratic discourse.
    DiscourseTurnRecorded {
        turn_id: Uuid,
        context_key: String,
        act: String,
    },
    /// A rule was triggered.
    RuleTriggered { rule_id: NodeId, result: RuleResult },
    /// A review stop point was reached.
    ReviewRequested { review_id: NodeId, task_id: NodeId },
    /// An immutable advisory review draft was generated.
    ReviewDrafted {
        draft_id: Uuid,
        job_id: JobId,
        perspective: String,
    },
    /// A human edited review drafts and made the authoritative decision.
    ReviewResolved {
        resolution_id: Uuid,
        job_id: JobId,
        selected_draft_id: Option<Uuid>,
    },
    /// A task started executing.
    TaskStarted {
        task_id: NodeId,
        description: String,
    },
    /// A task completed successfully.
    TaskCompleted {
        task_id: NodeId,
        description: String,
    },
    /// A task failed during execution.
    TaskFailed {
        task_id: NodeId,
        description: String,
        reason: String,
    },
    /// A plan stop point was reached and requires human approval.
    StopPointReached { task_id: NodeId, reason: String },
    /// A deployment happened.
    Deployed { target: String, node_id: NodeId },
    /// A feature was proposed and approved by the user.
    FeatureProposed {
        title: String,
        plan_path: String,
        task_ids: Vec<String>,
    },
    /// A database snapshot was created.
    SnapshotCreated { name: String, path: String },
    /// A database snapshot was restored.
    SnapshotRestored { name: String, path: String },
    /// Raw model output crossed the digest boundary.
    ModelOutputDigested {
        context: String,
        status: String,
        repairs: Vec<RepairRecord>,
        questions: Vec<QuestionRecord>,
        answers: Vec<AnswerRecord>,
    },
    /// One job's evidence payload was erased under its governed retention
    /// policy. The job uuid is a pointer into retained append-only history
    /// (the `jobs` row survives), never a derivative of erased content.
    EvidenceErased {
        job_id: JobId,
        request_id: String,
        status: String,
        receipt: String,
        store_receipts: Vec<String>,
    },
    /// A retention sweep classified all governed evidence. Counts only.
    RetentionSwept {
        enforced: bool,
        delete_due: usize,
        deleted: usize,
        deferred: usize,
        review_due: usize,
        retained: usize,
        preserved: usize,
        quarantined: usize,
        orphan_blobs_removed: usize,
    },
    /// `reconcile-plan --apply` repaired plan/graph identity: legacy
    /// positional task keys were migrated to stable ids and/or derived
    /// ids were persisted into the plan file as explicit tags.
    PlanReconciled {
        plan_path: String,
        migrated_keys: Vec<KeyMigrationRecord>,
        persisted_ids: Vec<String>,
    },
}

/// One durable task-key rename applied by plan reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyMigrationRecord {
    pub old_key: String,
    pub new_key: String,
}

/// Serde mirror of a digest-boundary repair. Foundry-core stays free of the
/// digest dependency; only the stable code and human wording are retained.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RepairRecord {
    pub code: String,
    pub detail: String,
}

/// Serde mirror of a digest-boundary question raised for genuine ambiguity.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuestionRecord {
    pub path: String,
    pub prompt: String,
    pub candidates: Vec<String>,
}

/// An applied clarification: an accountable human decision, kept separate
/// from repairs exactly as the digest ledger keeps them separate.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AnswerRecord {
    pub path: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleResult {
    Pass,
    Fail { reason: String },
    Warn { reason: String },
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Event::NodeCreated { .. } => "node_created",
            Event::EdgeCreated { .. } => "edge_created",
            Event::TaskPlanned { .. } => "task_planned",
            Event::PlanIndexed { .. } => "plan_indexed",
            Event::CodeIndexed { .. } => "code_indexed",
            Event::ModelInvoked { .. } => "model_invoked",
            Event::DiscourseTurnRecorded { .. } => "discourse_turn_recorded",
            Event::RuleTriggered { .. } => "rule_triggered",
            Event::ReviewRequested { .. } => "review_requested",
            Event::ReviewDrafted { .. } => "review_drafted",
            Event::ReviewResolved { .. } => "review_resolved",
            Event::TaskStarted { .. } => "task_started",
            Event::TaskCompleted { .. } => "task_completed",
            Event::TaskFailed { .. } => "task_failed",
            Event::StopPointReached { .. } => "stop_point_reached",
            Event::Deployed { .. } => "deployed",
            Event::FeatureProposed { .. } => "feature_proposed",
            Event::SnapshotCreated { .. } => "snapshot_created",
            Event::SnapshotRestored { .. } => "snapshot_restored",
            Event::ModelOutputDigested { .. } => "model_output_digested",
            Event::EvidenceErased { .. } => "evidence_erased",
            Event::RetentionSwept { .. } => "retention_swept",
            Event::PlanReconciled { .. } => "plan_reconciled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_output_digested_round_trips_with_stable_kind() {
        let event = Event::ModelOutputDigested {
            context: "proposal".into(),
            status: "clarify".into(),
            repairs: vec![RepairRecord {
                code: "stripped_fence".into(),
                detail: "stripped a markdown fence".into(),
            }],
            questions: vec![QuestionRecord {
                path: "$.qty".into(),
                prompt: "which quantity?".into(),
                candidates: vec!["2".into(), "3".into()],
            }],
            answers: vec![AnswerRecord {
                path: "$.qty".into(),
                value: serde_json::json!(2),
            }],
        };
        assert_eq!(event.kind(), "model_output_digested");
        let json = serde_json::to_string(&event).expect("serializes");
        assert!(json.contains("\"event\":\"model_output_digested\""));
        let back: Event = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back.kind(), event.kind());
    }

    #[test]
    fn retention_events_round_trip_with_stable_kinds() {
        let erased = Event::EvidenceErased {
            job_id: JobId(uuid::Uuid::nil()),
            request_id: "sweep-x".into(),
            status: "complete".into(),
            receipt: "lethe://request/0".into(),
            store_receipts: vec!["job-results:erased:foundry://erasure/x".into()],
        };
        assert_eq!(erased.kind(), "evidence_erased");
        let swept = Event::RetentionSwept {
            enforced: true,
            delete_due: 1,
            deleted: 1,
            deferred: 0,
            review_due: 2,
            retained: 3,
            preserved: 1,
            quarantined: 0,
            orphan_blobs_removed: 0,
        };
        assert_eq!(swept.kind(), "retention_swept");
        for event in [erased, swept] {
            let json = serde_json::to_string(&event).expect("serializes");
            let back: Event = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(back.kind(), event.kind());
        }
    }
}
