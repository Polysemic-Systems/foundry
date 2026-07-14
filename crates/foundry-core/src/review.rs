use crate::{JobId, ReviewDecision};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Independent advisory perspective used to prepare a human review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPerspective {
    Evidence,
    Adversarial,
}

impl ReviewPerspective {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Evidence => "evidence",
            Self::Adversarial => "adversarial",
        }
    }
}

/// Immutable model-generated advice. It has no authority to transition state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDraft {
    pub id: Uuid,
    pub task_key: String,
    pub job_id: JobId,
    pub perspective: ReviewPerspective,
    pub recommendation: ReviewDecision,
    pub body: String,
    pub agent: String,
    pub created_at: DateTime<Utc>,
}

/// Provenance for the final, attributable human judgment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewResolution {
    pub id: Uuid,
    pub task_key: String,
    pub job_id: JobId,
    pub selected_draft_id: Option<Uuid>,
    pub original_draft: Option<String>,
    pub final_body: String,
    pub edit_similarity: Option<f64>,
    pub decision: ReviewDecision,
    pub reviewer: String,
    pub created_at: DateTime<Utc>,
}
