//! Typed, graph-native records of Socratic interaction.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The accountable speaker behind a discourse turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscourseSpeaker {
    Human,
    SocraticPartner,
    System,
}

impl DiscourseSpeaker {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::SocraticPartner => "socratic_partner",
            Self::System => "system",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "human" => Some(Self::Human),
            "socratic_partner" => Some(Self::SocraticPartner),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// The epistemic function of a turn, kept separate from its author.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscourseAct {
    Question,
    Observation,
    Assumption,
    Challenge,
    Synthesis,
}

impl DiscourseAct {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Observation => "observation",
            Self::Assumption => "assumption",
            Self::Challenge => "challenge",
            Self::Synthesis => "synthesis",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "question" => Some(Self::Question),
            "observation" => Some(Self::Observation),
            "assumption" => Some(Self::Assumption),
            "challenge" => Some(Self::Challenge),
            "synthesis" => Some(Self::Synthesis),
            _ => None,
        }
    }
}

/// One immutable turn in a contextual, reply-linked discourse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscourseTurn {
    pub id: Uuid,
    pub context_key: String,
    pub speaker: DiscourseSpeaker,
    pub act: DiscourseAct,
    pub body: String,
    pub reply_to: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

impl DiscourseTurn {
    pub fn new(
        context_key: impl Into<String>,
        speaker: DiscourseSpeaker,
        act: DiscourseAct,
        body: impl Into<String>,
        reply_to: Option<Uuid>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            context_key: context_key.into(),
            speaker,
            act,
            body: body.into(),
            reply_to,
            created_at: Utc::now(),
        }
    }
}
