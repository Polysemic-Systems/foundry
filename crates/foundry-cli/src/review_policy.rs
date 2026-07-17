use crate::runner;
use foundry_core::{JobResult, JobState, ReviewDecision, ReviewDraft, ReviewPerspective};

pub fn deterministic_evidence_review(task_key: &str, result: &JobResult) -> ReviewDraft {
    let mut observations = Vec::new();
    let mut blockers = Vec::new();
    if result.state == JobState::Succeeded {
        observations.push("The verification job reached succeeded.".to_string());
    } else {
        blockers.push(format!(
            "The job state is {}, not succeeded.",
            result.state.as_str()
        ));
    }
    if result.tests.is_empty() {
        blockers.push("No structured test result was captured.".into());
    } else if result.tests.iter().all(|test| test.passed) {
        observations.push(format!(
            "All {} structured test runs passed.",
            result.tests.len()
        ));
    } else {
        blockers.push("At least one structured test run failed.".into());
    }
    match result.change_set.as_ref() {
        Some(change_set) if change_set.files.is_empty() => {
            blockers.push("The change set contains no files.".into());
        }
        Some(change_set) => {
            observations.push(format!(
                "The SHA-256-addressed change set contains {} files.",
                change_set.files.len()
            ));
            if runner::patch_digest(&change_set.files) != change_set.patch_digest {
                blockers
                    .push("The aggregate patch digest does not match its file evidence.".into());
            }
            if result.staged
                && change_set.files.iter().any(|change| {
                    !matches!(
                        (
                            change.status,
                            change.before.is_some(),
                            change.after.is_some()
                        ),
                        (foundry_core::ChangeStatus::Added, false, true)
                            | (foundry_core::ChangeStatus::Modified, true, true)
                            | (foundry_core::ChangeStatus::Deleted, true, false)
                    )
                })
            {
                blockers.push(
                    "The staged patch is not content-complete and cannot be promoted.".into(),
                );
            }
        }
        None => blockers.push("No change set was captured.".into()),
    }
    match result.executor_image.as_deref() {
        Some(image) if !image.starts_with("unresolved:") => {
            observations.push(format!("The executor image is identified as {image}."));
        }
        _ => blockers.push("The executor image is not resolved to an immutable local ID.".into()),
    }
    match result.acceptance_authority.as_deref() {
        Some(foundry_core::job::acceptance_authority::RED_PHASE) => {
            observations.push("The acceptance check was observed failing before it passed.".into());
        }
        Some(foundry_core::job::acceptance_authority::UNFALSIFIED) => {
            observations.push(
                "WARNING: the acceptance check was NEVER observed failing; \
                 this pass may be vacuous (it could succeed with the task unimplemented)."
                    .into(),
            );
        }
        Some(other) => {
            observations.push(format!("Unrecognized acceptance authority: {other}."));
        }
        None => {
            observations
                .push("The record predates acceptance-authority tracking (unknown).".into());
        }
    }
    let recommendation = if blockers.is_empty() {
        ReviewDecision::Approve
    } else {
        ReviewDecision::Reject
    };
    let body = format!(
        "RECOMMENDATION: {}\n\nShared question\nDoes the immutable evidence satisfy the mechanical promotion policy for {task_key}?\n\nObserved evidence\n{}\n\nAssumptions\nPassing captured tests are necessary but not sufficient to prove semantic correctness.\n\nCompeting interpretation\nA mechanically complete patch may still implement the wrong behavior.\n\nFalsifying evidence\n{}\n\nQuestion for the human\nDo the recorded bytes and acceptance criteria establish the intended behavior beyond these mechanical checks?\n\nSynthesis\n{}",
        match recommendation {
            ReviewDecision::Approve => "APPROVE",
            ReviewDecision::Reject => "REJECT",
        },
        if observations.is_empty() {
            "(none)".into()
        } else {
            observations.join("\n")
        },
        if blockers.is_empty() {
            "A conflicting workspace state or adversarial semantic counterexample would overturn this recommendation.".into()
        } else {
            blockers.join("\n")
        },
        if blockers.is_empty() {
            "Mechanical evidence is complete; continue to adversarial and human semantic review."
        } else {
            "Mechanical evidence is incomplete; do not promote until every blocker is resolved."
        }
    );
    ReviewDraft {
        id: uuid::Uuid::new_v4(),
        task_key: task_key.into(),
        job_id: result.job_id,
        perspective: ReviewPerspective::Evidence,
        recommendation,
        body,
        agent: "foundry-deterministic-evidence-policy/v1".into(),
        created_at: chrono::Utc::now(),
    }
}

pub fn format_change_evidence(change_set: &foundry_core::ChangeSet) -> String {
    const SIDE_LIMIT: usize = 2_000;
    const TOTAL_LIMIT: usize = 16_000;
    let mut rendered = format!(
        "Base: {}\nPatch: {}\n",
        change_set.base_revision, change_set.patch_digest
    );
    for change in &change_set.files {
        let section = format!(
            "\n=== {:?}: {} ===\nBEFORE {}\n{}\nAFTER {}\n{}\n",
            change.status,
            change.path,
            change
                .before
                .as_ref()
                .map(|evidence| evidence.digest.as_str())
                .unwrap_or("(absent)"),
            evidence_preview(change.before.as_ref(), SIDE_LIMIT),
            change
                .after
                .as_ref()
                .map(|evidence| evidence.digest.as_str())
                .unwrap_or("(absent)"),
            evidence_preview(change.after.as_ref(), SIDE_LIMIT),
        );
        if rendered.chars().count() + section.chars().count() > TOTAL_LIMIT {
            rendered.push_str(
                "\n(change evidence preview truncated; complete bytes remain in the verified content-addressed evidence store)\n",
            );
            break;
        }
        rendered.push_str(&section);
    }
    rendered
}

fn evidence_preview(evidence: Option<&foundry_core::FileEvidence>, limit: usize) -> String {
    let Some(evidence) = evidence else {
        return "(absent)".into();
    };
    match std::str::from_utf8(&evidence.bytes) {
        Ok(text) => {
            let mut preview = text.chars().take(limit).collect::<String>();
            if text.chars().count() > limit {
                preview.push_str("\n… (preview truncated)");
            }
            preview
        }
        Err(_) => format!("(binary content, {} bytes)", evidence.bytes.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::{
        ChangeSet, ChangeStatus, ChangedFile, FileEvidence, GovernanceEnvelope, JobId, JobResult,
        JobState, KnowledgeLayer, RetentionPolicy, SourceRef, Transformation,
        job::acceptance_authority,
    };

    fn result_with_authority(authority: Option<&str>) -> JobResult {
        let governance = GovernanceEnvelope {
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
            owner: "review-policy-tests".into(),
            retention: RetentionPolicy::Preserve {
                basis: "test evidence".into(),
            },
        };
        let mut result = JobResult::new(JobId::new(), JobState::Succeeded, governance).unwrap();
        result.acceptance_authority = authority.map(str::to_string);
        result
    }

    #[test]
    fn unfalsified_evidence_is_loudly_marked_in_the_draft() {
        let result = result_with_authority(Some(acceptance_authority::UNFALSIFIED));
        let draft = deterministic_evidence_review("plans/f.plan.md#some-task", &result);
        assert!(
            draft.body.contains("NEVER observed failing"),
            "the draft must force review visibility of vacuous evidence: {}",
            draft.body
        );
    }

    #[test]
    fn red_phase_authority_is_reported_as_an_observation() {
        let result = result_with_authority(Some(acceptance_authority::RED_PHASE));
        let draft = deterministic_evidence_review("plans/f.plan.md#some-task", &result);
        assert!(
            draft.body.contains("observed failing before it passed"),
            "{}",
            draft.body
        );
        assert!(!draft.body.contains("NEVER observed failing"));
    }

    #[test]
    fn legacy_records_without_authority_are_reported_as_unknown() {
        let result = result_with_authority(None);
        let draft = deterministic_evidence_review("plans/f.plan.md#some-task", &result);
        assert!(
            draft
                .body
                .contains("predates acceptance-authority tracking"),
            "{}",
            draft.body
        );
    }

    #[test]
    fn evidence_contains_recorded_before_and_after_content() {
        let evidence = |value: &str| FileEvidence {
            digest: runner::sha256_digest(value.as_bytes()),
            bytes: value.as_bytes().to_vec(),
            blob: None,
            executable: false,
        };
        let rendered = format_change_evidence(&ChangeSet {
            base_revision: "sha256:base".into(),
            patch_digest: "sha256:patch".into(),
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                status: ChangeStatus::Modified,
                before: Some(evidence("unsafe old behavior")),
                after: Some(evidence("verified new behavior")),
            }],
        });

        assert!(rendered.contains("unsafe old behavior"));
        assert!(rendered.contains("verified new behavior"));
        assert!(rendered.contains("sha256:"));
    }
}
