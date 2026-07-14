use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

use crate::living::{ConformanceError, GovernanceEnvelope};

/// Lifecycle of a planned unit of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Proposed,
    Ready,
    Running,
    Blocked,
    Review,
    Done,
    Failed,
}

impl TaskState {
    pub fn transition(self, next: Self) -> Result<Self, TransitionError> {
        let valid = matches!(
            (self, next),
            (Self::Proposed, Self::Ready)
                | (Self::Ready, Self::Running)
                | (Self::Running, Self::Blocked | Self::Review | Self::Failed)
                | (Self::Blocked, Self::Ready | Self::Failed)
                | (Self::Review, Self::Done | Self::Ready | Self::Failed)
                | (Self::Failed, Self::Ready)
        );
        valid.then_some(next).ok_or(TransitionError {
            lifecycle: "task",
            from: format!("{self:?}"),
            to: format!("{next:?}"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Review => "review",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

impl FromStr for TaskState {
    type Err = StateParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "proposed" => Ok(Self::Proposed),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "blocked" => Ok(Self::Blocked),
            "review" => Ok(Self::Review),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            _ => Err(StateParseError(value.to_string())),
        }
    }
}

/// Lifecycle of one execution attempt for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

impl JobState {
    pub fn transition(self, next: Self) -> Result<Self, TransitionError> {
        let valid = matches!(
            (self, next),
            (Self::Queued, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut
                )
        );
        valid.then_some(next).ok_or(TransitionError {
            lifecycle: "job",
            from: format!("{self:?}"),
            to: format!("{next:?}"),
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
}

impl FromStr for JobState {
    type Err = StateParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "timed_out" => Ok(Self::TimedOut),
            _ => Err(StateParseError(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub Uuid);

impl JobId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    /// Stable plan-relative task key, independent from rebuilt graph node IDs.
    pub task_key: String,
    pub idempotency_key: String,
    pub state: JobState,
}

/// Reproducible input to an isolated runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    pub command: Vec<String>,
    pub working_directory: String,
    pub environment: BTreeMap<String, String>,
    pub timeout_seconds: u64,
    pub cpu_limit: Option<u32>,
    pub memory_limit_bytes: Option<u64>,
    pub network_enabled: bool,
}

impl JobSpec {
    /// Deterministic Podman arguments for a non-interactive, resource-bounded run.
    pub fn podman_args(&self, image: &str, root: &str, name: &str) -> Vec<String> {
        let mut args = vec![
            "run".into(),
            "--rm".into(),
            "--name".into(),
            name.into(),
            "--userns=keep-id".into(),
            "--workdir".into(),
            self.working_directory.clone(),
            "--volume".into(),
            format!("{root}:/workspace:Z"),
        ];
        args.push(if self.network_enabled {
            "--network=slirp4netns".into()
        } else {
            "--network=none".into()
        });
        if let Some(cpu) = self.cpu_limit {
            args.extend(["--cpus".into(), cpu.to_string()]);
        }
        if let Some(memory) = self.memory_limit_bytes {
            args.extend(["--memory".into(), memory.to_string()]);
        }
        for (key, value) in &self.environment {
            args.extend(["--env".into(), format!("{key}={value}")]);
        }
        args.push(image.into());
        args.extend(self.command.iter().cloned());
        args
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub path: String,
    /// Algorithm-qualified digest, for example `sha256:abc...`.
    pub digest: String,
    pub size_bytes: u64,
    pub governance: GovernanceEnvelope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: ChangeStatus,
    /// Immutable content before the attempt. Missing for newly added files and
    /// legacy job records captured before content-complete evidence existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<FileEvidence>,
    /// Immutable content after the attempt. Missing for deleted files and
    /// legacy job records captured before content-complete evidence existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<FileEvidence>,
}

/// Reproducible evidence for one regular workspace file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEvidence {
    /// Algorithm-qualified digest of `bytes`.
    pub digest: String,
    /// Inline content retained by legacy and in-memory records. File-backed
    /// graphs externalize this into `blob` before writing the SQLite row.
    #[serde(default)]
    pub bytes: Vec<u8>,
    /// Content-addressed object reference, normally identical to `digest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    pub executable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub base_revision: String,
    pub patch_digest: String,
    pub files: Vec<ChangedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResult {
    pub command: Vec<String>,
    pub passed: bool,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub task_key: String,
    pub job_id: JobId,
    pub decision: ReviewDecision,
    pub reviewer: String,
    pub reason: String,
}

/// Immutable outcome captured after a job reaches a terminal state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResult {
    pub job_id: JobId,
    pub state: JobState,
    pub spec: Option<JobSpec>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub artifacts: Vec<Artifact>,
    pub tests: Vec<TestResult>,
    pub change_set: Option<ChangeSet>,
    /// Exact container image identity when the local runtime can resolve it;
    /// otherwise an explicitly `unresolved:` image reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_image: Option<String>,
    /// Staged jobs ran outside the authoritative workspace. Their change set
    /// must be promoted only by an approving human review.
    #[serde(default)]
    pub staged: bool,
    pub governance: GovernanceEnvelope,
}

impl JobResult {
    pub fn new(
        job_id: JobId,
        state: JobState,
        governance: GovernanceEnvelope,
    ) -> Result<Self, JobContractError> {
        if !state.is_terminal() {
            return Err(JobContractError::NonTerminalResult(state));
        }
        governance.validate()?;
        Ok(Self {
            job_id,
            state,
            spec: None,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 0,
            artifacts: Vec::new(),
            tests: Vec::new(),
            change_set: None,
            executor_image: None,
            staged: false,
            governance,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JobContractError {
    #[error("cannot create a final result for non-terminal job state {0:?}")]
    NonTerminalResult(JobState),
    #[error(transparent)]
    NonconformingEvidence(#[from] ConformanceError),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown lifecycle state: {0}")]
pub struct StateParseError(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid {lifecycle} transition from {from} to {to}")]
pub struct TransitionError {
    pub lifecycle: &'static str,
    pub from: String,
    pub to: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::living::{
        KnowledgeLayer, NamedAssumption, RetentionPolicy, SourceRef, Transformation,
    };

    fn governance() -> GovernanceEnvelope {
        GovernanceEnvelope {
            layer: KnowledgeLayer::Observed,
            sources: vec![SourceRef {
                uri: "job://test".into(),
                digest: None,
            }],
            assumptions: vec![NamedAssumption {
                name: "test fixture".into(),
                statement: "the fixture represents runner output".into(),
            }],
            transformation: Transformation {
                name: "test".into(),
                version: "1".into(),
                input_digests: Vec::new(),
            },
            owner: "foundry-core-tests".into(),
            retention: RetentionPolicy::Preserve {
                basis: "regression test".into(),
            },
        }
    }

    #[test]
    fn task_requires_review_before_done() {
        assert!(TaskState::Running.transition(TaskState::Done).is_err());
        assert_eq!(
            TaskState::Running.transition(TaskState::Review),
            Ok(TaskState::Review)
        );
        assert_eq!(
            TaskState::Review.transition(TaskState::Done),
            Ok(TaskState::Done)
        );
    }

    #[test]
    fn rejected_task_can_return_to_ready() {
        assert_eq!(
            TaskState::Review.transition(TaskState::Ready),
            Ok(TaskState::Ready)
        );
    }

    #[test]
    fn terminal_job_cannot_restart() {
        for state in [
            JobState::Succeeded,
            JobState::Failed,
            JobState::Cancelled,
            JobState::TimedOut,
        ] {
            assert!(state.is_terminal());
            assert!(state.transition(JobState::Running).is_err());
        }
    }

    #[test]
    fn queued_job_can_be_cancelled_without_running() {
        assert_eq!(
            JobState::Queued.transition(JobState::Cancelled),
            Ok(JobState::Cancelled)
        );
    }

    #[test]
    fn job_contracts_reject_non_terminal_results() {
        assert!(matches!(
            JobResult::new(JobId::new(), JobState::Running, governance()),
            Err(JobContractError::NonTerminalResult(JobState::Running))
        ));
    }

    #[test]
    fn job_contracts_round_trip_as_json() {
        let spec = JobSpec {
            command: vec!["cargo".into(), "test".into()],
            working_directory: "/workspace".into(),
            environment: BTreeMap::from([("RUST_BACKTRACE".into(), "1".into())]),
            timeout_seconds: 300,
            cpu_limit: Some(2),
            memory_limit_bytes: Some(1_073_741_824),
            network_enabled: false,
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<JobSpec>(&json).unwrap(), spec);

        let result = JobResult::new(JobId::new(), JobState::Succeeded, governance()).unwrap();
        let json = serde_json::to_string(&result).unwrap();
        assert_eq!(serde_json::from_str::<JobResult>(&json).unwrap(), result);
    }

    #[test]
    fn podman_contract_is_non_interactive_and_bounded() {
        let spec = JobSpec {
            command: vec!["cargo".into(), "test".into()],
            working_directory: "/workspace".into(),
            environment: BTreeMap::new(),
            timeout_seconds: 60,
            cpu_limit: Some(2),
            memory_limit_bytes: Some(1024),
            network_enabled: false,
        };
        let args = spec.podman_args("rust:1", "/project", "job-1");
        assert!(!args.iter().any(|arg| arg == "-it" || arg == "-i"));
        assert!(args.iter().any(|arg| arg == "--network=none"));
        assert!(args.windows(2).any(|pair| pair == ["--cpus", "2"]));
        assert!(args.windows(2).any(|pair| pair == ["--memory", "1024"]));
        assert_eq!(&args[args.len() - 2..], ["cargo", "test"]);
    }
}
