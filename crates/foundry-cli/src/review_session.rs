use anyhow::{Context, Result};
use foundry_core::{ReviewDecision, ReviewDraft, ReviewPerspective};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewChoice {
    Evidence,
    Adversarial,
    Both,
    Neither,
}

impl ReviewChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::Evidence => "evidence review",
            Self::Adversarial => "adversarial review",
            Self::Both => "both reviews",
            Self::Neither => "neither review",
        }
    }
}

pub struct ReviewAnswer<'a> {
    pub choice: Option<ReviewChoice>,
    pub decision: ReviewDecision,
    pub decision_confirmed: bool,
    pub rationale: &'a str,
    pub custom_entry: &'a str,
}

pub fn validation_error(answer: &ReviewAnswer<'_>, drafts: &[ReviewDraft]) -> Option<String> {
    let mut missing = Vec::new();
    if answer.choice.is_none() {
        missing.push("choose evidence, adversarial, both, or neither");
    }
    if !answer.decision_confirmed {
        missing.push("explicitly confirm an approve or reject decision");
    }

    let rationale = answer.rationale.trim();
    if rationale.chars().count() < 20 || rationale.split_whitespace().count() < 4 {
        missing.push("write a rationale of at least four words and 20 characters");
    } else if drafts
        .iter()
        .any(|draft| normalized(&draft.body) == normalized(rationale))
    {
        missing.push("write the rationale in your own words instead of copying an advisory draft");
    }

    if missing.is_empty() {
        None
    } else {
        Some(format!("Cannot sign yet: {}.", missing.join("; ")))
    }
}

pub fn render_resolution(answer: &ReviewAnswer<'_>, drafts: &[ReviewDraft]) -> Result<String> {
    let choice = answer
        .choice
        .context("a review resolution requires an explicit advisory choice")?;
    let decision = match answer.decision {
        ReviewDecision::Approve => "APPROVE",
        ReviewDecision::Reject => "REJECT",
    };
    let custom_entry = answer.custom_entry.trim();
    let challenge = decision_challenge(drafts);
    Ok(format!(
        "# Human review resolution\n\n\
         Advisory answer: {}\n\
         Decision: {decision}\n\n\
         ## Decision-bearing question\n\n\
         {challenge}\n\n\
         ## Reasoned answer\n\n\
         {}\n\n\
         ## Questions or ideas\n\n\
         {}",
        choice.label(),
        answer.rationale.trim(),
        if custom_entry.is_empty() {
            "(none)"
        } else {
            custom_entry
        }
    ))
}

pub fn selected_draft(
    choice: Option<ReviewChoice>,
    drafts: &[ReviewDraft],
) -> Option<&ReviewDraft> {
    let perspective = match choice? {
        ReviewChoice::Evidence => ReviewPerspective::Evidence,
        ReviewChoice::Adversarial => ReviewPerspective::Adversarial,
        ReviewChoice::Both | ReviewChoice::Neither => return None,
    };
    drafts.iter().find(|draft| draft.perspective == perspective)
}

pub fn decision_challenge(drafts: &[ReviewDraft]) -> &'static str {
    let mut recommendations = drafts.iter().map(|draft| draft.recommendation);
    match (recommendations.next(), recommendations.next()) {
        (Some(first), Some(second)) if first == second => {
            "What failure mode could make the reviews' shared recommendation wrong?"
        }
        _ => "What evidence resolves the reviews' disagreement?",
    }
}

fn normalized(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use foundry_core::{JobId, ReviewDraft};
    use uuid::Uuid;

    fn draft(perspective: ReviewPerspective, body: &str) -> ReviewDraft {
        ReviewDraft {
            id: Uuid::new_v4(),
            task_key: "plans/features.plan.md#review-session".into(),
            job_id: JobId(Uuid::new_v4()),
            perspective,
            recommendation: match perspective {
                ReviewPerspective::Evidence => ReviewDecision::Approve,
                ReviewPerspective::Adversarial => ReviewDecision::Reject,
            },
            body: body.into(),
            agent: "test".into(),
            created_at: Utc::now(),
        }
    }

    fn drafts() -> Vec<ReviewDraft> {
        vec![
            draft(
                ReviewPerspective::Evidence,
                "The acceptance evidence is complete and supports approval.",
            ),
            draft(
                ReviewPerspective::Adversarial,
                "A compatibility risk remains and supports rejection.",
            ),
        ]
    }

    #[test]
    fn choosing_a_generated_review_does_not_earn_signing_authority() {
        let drafts = drafts();
        let answer = ReviewAnswer {
            choice: Some(ReviewChoice::Evidence),
            decision: ReviewDecision::Approve,
            decision_confirmed: false,
            rationale: "",
            custom_entry: "",
        };

        let error = validation_error(&answer, &drafts).expect("incomplete answer must be refused");
        assert!(error.contains("decision"));
        assert!(error.contains("rationale"));
    }

    #[test]
    fn copied_advisory_prose_is_not_a_human_answer() {
        let drafts = drafts();
        let answer = ReviewAnswer {
            choice: Some(ReviewChoice::Evidence),
            decision: ReviewDecision::Approve,
            decision_confirmed: true,
            rationale: &drafts[0].body,
            custom_entry: "",
        };

        let error = validation_error(&answer, &drafts).expect("copied draft must be refused");
        assert!(error.contains("own words"));
    }

    #[test]
    fn explicit_choice_decision_and_reasoned_answer_can_be_signed() {
        let drafts = drafts();
        let answer = ReviewAnswer {
            choice: Some(ReviewChoice::Both),
            decision: ReviewDecision::Reject,
            decision_confirmed: true,
            rationale: "The acceptance output is credible, but compatibility remains untested and blocks release.",
            custom_entry: "Could the next attempt add a backwards-compatibility fixture?",
        };

        assert_eq!(validation_error(&answer, &drafts), None);
        assert!(selected_draft(answer.choice, &drafts).is_none());
        let body = render_resolution(&answer, &drafts).unwrap();
        assert!(body.contains("Advisory answer: both reviews"));
        assert!(body.contains("Decision: REJECT"));
        assert!(body.contains(answer.rationale));
        assert!(body.contains(answer.custom_entry));
    }

    #[test]
    fn a_single_advisory_choice_preserves_provenance_without_copying_it() {
        let drafts = drafts();
        let answer = ReviewAnswer {
            choice: Some(ReviewChoice::Adversarial),
            decision: ReviewDecision::Reject,
            decision_confirmed: true,
            rationale: "The identified compatibility gap is material because the public format is persistent.",
            custom_entry: "",
        };

        assert_eq!(
            selected_draft(answer.choice, &drafts).map(|draft| draft.id),
            Some(drafts[1].id)
        );
        let body = render_resolution(&answer, &drafts).unwrap();
        assert!(!body.contains(&drafts[1].body));
        assert!(body.contains("Questions or ideas\n\n(none)"));
    }
}
