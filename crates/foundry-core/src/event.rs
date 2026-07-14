use crate::graph::NodeId;

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
    /// A rule was triggered.
    RuleTriggered { rule_id: NodeId, result: RuleResult },
    /// A review stop point was reached.
    ReviewRequested { review_id: NodeId, task_id: NodeId },
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
            Event::RuleTriggered { .. } => "rule_triggered",
            Event::ReviewRequested { .. } => "review_requested",
            Event::TaskStarted { .. } => "task_started",
            Event::TaskCompleted { .. } => "task_completed",
            Event::TaskFailed { .. } => "task_failed",
            Event::StopPointReached { .. } => "stop_point_reached",
            Event::Deployed { .. } => "deployed",
            Event::FeatureProposed { .. } => "feature_proposed",
            Event::SnapshotCreated { .. } => "snapshot_created",
            Event::SnapshotRestored { .. } => "snapshot_restored",
        }
    }
}
