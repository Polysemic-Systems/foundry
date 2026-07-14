//! Foundry core: the graph, events, and plan primitives.
//!
//! This crate is the spec. The domain types here are the only truth.

pub mod discourse;
pub mod embed;
pub mod event;
pub mod evidence_store;
pub mod graph;
pub mod job;
pub mod living;
pub mod plan;
pub mod review;
pub mod rules;
pub mod search;

pub mod db;

pub mod migration_registry;

pub mod migration_storage;

pub use discourse::{DiscourseAct, DiscourseSpeaker, DiscourseTurn};
pub use embed::{cosine_similarity, normalize};
pub use event::{Event, RuleResult};
pub use graph::{Edge, EdgeKind, Graph, Node, NodeKind};
pub use job::{
    Artifact, ChangeSet, ChangeStatus, ChangedFile, FileEvidence, Job, JobContractError, JobId,
    JobResult, JobSpec, JobState, Review, ReviewDecision, StateParseError, TaskState, TestResult,
    TransitionError,
};
pub use living::{
    ConformanceError, Disposition, GovernanceEnvelope, KnowledgeLayer, NamedAssumption,
    RetentionPolicy, SourceRef, Transformation,
};
pub use plan::Plan;
pub use review::{ReviewDraft, ReviewPerspective, ReviewResolution};
pub use rules::{Rule, built_in_rules};
pub use search::sanitize_query;
