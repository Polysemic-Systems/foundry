//! The digest boundary: every model-generated payload crosses here before
//! Foundry trusts its shape. Repairs are named, ambiguity becomes questions,
//! and each crossing is recorded as a `ModelOutputDigested` event.

use anyhow::{Context, Result, bail};
use foundry_core::{AnswerRecord, Event, QuestionRecord, RepairRecord, ReviewDecision};
use polysemic_core::Value as PValue;
use polysemic_digest::{Answer, Digestion, Field, Outcome, Question, Repair, Schema};
use std::io::Write;

/// A task proposed by the model: description, files, optional run command.
pub type ProposedTask = (String, Vec<String>, Option<String>);

// ---- Value bridge -------------------------------------------------------

/// polysemic Value -> serde_json. Infallible: a non-finite number cannot come
/// out of the strict parser, but a constructed one maps to Null rather than
/// panicking inside serde_json.
pub fn to_serde(value: &PValue) -> serde_json::Value {
    match value {
        PValue::Null => serde_json::Value::Null,
        PValue::Bool(b) => serde_json::Value::Bool(*b),
        PValue::Num(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        PValue::Str(s) => serde_json::Value::String(s.clone()),
        PValue::Arr(items) => serde_json::Value::Array(items.iter().map(to_serde).collect()),
        PValue::Obj(map) => {
            serde_json::Value::Object(map.iter().map(|(k, v)| (k.clone(), to_serde(v))).collect())
        }
    }
}

/// serde_json -> polysemic Value. Numbers go through f64, which is lossy
/// above 2^53; neither boundary schema carries numeric fields today. Only the
/// round-trip test exercises this direction so far — promote it out of
/// `cfg(test)` when a production caller needs to hand digest a serde value.
#[cfg(test)]
pub fn from_serde(value: &serde_json::Value) -> PValue {
    match value {
        serde_json::Value::Null => PValue::Null,
        serde_json::Value::Bool(b) => PValue::Bool(*b),
        serde_json::Value::Number(n) => PValue::Num(n.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::String(s) => PValue::Str(s.clone()),
        serde_json::Value::Array(items) => PValue::Arr(items.iter().map(from_serde).collect()),
        serde_json::Value::Object(map) => PValue::Obj(
            map.iter()
                .map(|(k, v)| (k.clone(), from_serde(v)))
                .collect(),
        ),
    }
}

// ---- Ledger mirrors ------------------------------------------------------

pub fn repair_records(repairs: &[Repair]) -> Vec<RepairRecord> {
    repairs
        .iter()
        .map(|repair| RepairRecord {
            code: repair.code().to_string(),
            detail: repair.to_string(),
        })
        .collect()
}

pub fn question_records(questions: &[Question]) -> Vec<QuestionRecord> {
    questions
        .iter()
        .map(|question| QuestionRecord {
            path: question.path.clone(),
            prompt: question.prompt.clone(),
            candidates: question.candidates.clone(),
        })
        .collect()
}

pub fn answer_records(answers: &[Answer]) -> Vec<AnswerRecord> {
    answers
        .iter()
        .map(|answer| AnswerRecord {
            path: answer.path.clone(),
            value: to_serde(&answer.value),
        })
        .collect()
}

// ---- Schemas -------------------------------------------------------------

/// The proposal contract: a spec plus an ordered task list. Deliberately no
/// task-number field — array order is task order.
pub fn proposal_schema() -> Schema {
    Schema::obj([
        Field::req("spec", Schema::Str),
        Field::req(
            "tasks",
            Schema::Arr(Box::new(Schema::obj([
                Field::req("description", Schema::Str),
                Field::opt("files", Schema::Arr(Box::new(Schema::Str))),
                Field::opt("run", Schema::Str),
            ]))),
        ),
    ])
}

/// The reviewer contract: a recommendation the schema can case-fold plus the
/// verbatim Socratic markdown body.
pub fn review_schema() -> Schema {
    Schema::obj([
        Field::req("recommendation", Schema::choice(["approve", "reject"])),
        Field::req("body", Schema::Str),
    ])
}

// ---- The boundary crossing ------------------------------------------------

#[derive(Debug)]
pub enum DigestStatus {
    Resolved(serde_json::Value),
    Clarify(Vec<Question>),
    Unparseable(String),
}

#[derive(Debug)]
pub struct DigestedOutput {
    pub status: DigestStatus,
    pub repairs: Vec<Repair>,
    /// Pre-built ModelOutputDigested event; the caller records it. Building
    /// without recording keeps this function pure and testable without a DB.
    pub event: Event,
}

/// One digestion pass. Model misbehavior never returns `Err` — an
/// unrecoverable parse becomes `DigestStatus::Unparseable` so the event is
/// still produced. `Err` is reserved for `AnswerError`: bad human input or a
/// caller bug (answering a path that was never questioned).
pub fn digest_model_output(
    context: &str,
    raw: &str,
    schema: &Schema,
    answers: Vec<Answer>,
) -> Result<DigestedOutput> {
    let digestion: Result<Digestion, String> = if answers.is_empty() {
        polysemic_digest::digest(raw, schema).map_err(|e| e.to_string())
    } else {
        match polysemic_digest::digest_with_answers(raw, schema, answers) {
            Ok(digestion) => Ok(digestion),
            Err(polysemic_digest::AnswerError::Parse(e)) => Err(e.to_string()),
            Err(answer_error) => {
                bail!("clarification answer rejected: {answer_error}");
            }
        }
    };
    Ok(match digestion {
        Ok(digestion) => {
            let repair_mirror = repair_records(&digestion.repairs);
            let answer_mirror = answer_records(&digestion.answers);
            match digestion.outcome {
                Outcome::Resolved(value) => DigestedOutput {
                    event: Event::ModelOutputDigested {
                        context: context.to_string(),
                        status: "resolved".to_string(),
                        repairs: repair_mirror,
                        questions: Vec::new(),
                        answers: answer_mirror,
                    },
                    status: DigestStatus::Resolved(to_serde(&value)),
                    repairs: digestion.repairs,
                },
                Outcome::Clarify(questions) => DigestedOutput {
                    event: Event::ModelOutputDigested {
                        context: context.to_string(),
                        status: "clarify".to_string(),
                        repairs: repair_mirror,
                        questions: question_records(&questions),
                        answers: answer_mirror,
                    },
                    status: DigestStatus::Clarify(questions),
                    repairs: digestion.repairs,
                },
            }
        }
        Err(reason) => DigestedOutput {
            event: Event::ModelOutputDigested {
                context: context.to_string(),
                status: "unparseable".to_string(),
                repairs: Vec::new(),
                questions: Vec::new(),
                answers: Vec::new(),
            },
            status: DigestStatus::Unparseable(reason),
            repairs: Vec::new(),
        },
    })
}

/// Print the named-repair ledger, one line per repair; silent when empty.
pub fn print_repair_ledger(context: &str, repairs: &[Repair]) {
    for repair in repairs {
        println!("digest[{context}]: {repair}");
    }
}

// ---- Extraction ------------------------------------------------------------

/// Pull (spec, tasks) out of a resolved proposal value. The schema already
/// guaranteed types; unknown extra keys were kept by digest and are simply
/// not read here. Errors are reserved for impossible shapes.
pub fn extract_proposal(value: &serde_json::Value) -> Result<(String, Vec<ProposedTask>)> {
    let spec = value
        .get("spec")
        .and_then(|v| v.as_str())
        .context("resolved proposal is missing the spec string")?
        .trim()
        .to_string();
    let tasks = value
        .get("tasks")
        .and_then(|v| v.as_array())
        .context("resolved proposal is missing the tasks array")?
        .iter()
        .map(|task| {
            let description = task
                .get("description")
                .and_then(|v| v.as_str())
                .context("resolved task is missing its description")?
                .trim()
                .to_string();
            let files = task
                .get("files")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let run = task
                .get("run")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Ok((description, files, run))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((spec, tasks))
}

/// Pull (decision, body) out of a resolved review envelope. The schema's
/// choice field already case-folded the recommendation.
pub fn extract_review(value: &serde_json::Value) -> Result<(ReviewDecision, String)> {
    let recommendation = value
        .get("recommendation")
        .and_then(|v| v.as_str())
        .context("resolved review is missing the recommendation")?;
    let decision = match recommendation {
        "approve" => ReviewDecision::Approve,
        "reject" => ReviewDecision::Reject,
        other => bail!("schema admitted an unknown recommendation: {other}"),
    };
    let body = value
        .get("body")
        .and_then(|v| v.as_str())
        .context("resolved review is missing the body")?
        .to_string();
    Ok((decision, body))
}

/// Ask the human each outstanding question on stdin. Empty line skips (the
/// question stays open); a bare candidate index picks that candidate; any
/// other input is parsed as JSON, falling back to a plain string.
pub fn collect_answers_interactively(questions: &[Question]) -> Result<Vec<Answer>> {
    let mut answers = Vec::new();
    for question in questions {
        println!("\n{question}");
        print!("answer (empty=skip, digit=pick candidate): ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        let value = input
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| question.candidates.get(index))
            .map(|candidate| {
                polysemic_core::parse(candidate).unwrap_or_else(|_| PValue::Str(candidate.clone()))
            })
            .unwrap_or_else(|| {
                polysemic_core::parse(input).unwrap_or_else(|_| PValue::Str(input.to_string()))
            });
        answers.push(Answer::new(&question.path, value));
    }
    Ok(answers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repair_codes(repairs: &[Repair]) -> Vec<&'static str> {
        repairs.iter().map(|r| r.code()).collect()
    }

    #[test]
    fn bridge_round_trips_nested_values() {
        let original = serde_json::json!({
            "spec": "a plan",
            "flag": true,
            "nothing": null,
            "qty": 2.0,
            "tasks": [{"description": "one", "files": ["a.rs", "b.rs"]}],
        });
        let bridged = to_serde(&from_serde(&original));
        assert_eq!(bridged, original);
    }

    #[test]
    fn mangled_proposal_resolves_with_named_repairs() {
        let raw = "Here is the plan you asked for:\n```json\n{'spec': 'Add a sweep command.', 'tasks': [{'description': 'Add sweep', 'files': ['crates/foundry-cli/src/main.rs'], 'run': 'cargo test',},],}\n```\nLet me know if you need anything else.";
        let digested = digest_model_output("proposal", raw, &proposal_schema(), vec![])
            .expect("no answer error");
        let DigestStatus::Resolved(value) = digested.status else {
            panic!("expected resolved, got repairs {:?}", digested.repairs);
        };
        let codes = repair_codes(&digested.repairs);
        assert!(codes.contains(&"stripped_fence"), "codes: {codes:?}");
        assert!(codes.contains(&"requoted_strings"), "codes: {codes:?}");
        assert!(
            codes.contains(&"removed_trailing_commas"),
            "codes: {codes:?}"
        );
        let (spec, tasks) = extract_proposal(&value).expect("extracts");
        assert_eq!(spec, "Add a sweep command.");
        assert_eq!(
            tasks,
            vec![(
                "Add sweep".to_string(),
                vec!["crates/foundry-cli/src/main.rs".to_string()],
                Some("cargo test".to_string()),
            )]
        );
        let Event::ModelOutputDigested {
            status, repairs, ..
        } = &digested.event
        else {
            panic!("wrong event kind");
        };
        assert_eq!(status, "resolved");
        assert!(!repairs.is_empty());
    }

    #[test]
    fn missing_spec_and_null_run_become_questions() {
        let raw = r#"{"tasks": [{"description": "one", "run": null}]}"#;
        let digested = digest_model_output("proposal", raw, &proposal_schema(), vec![])
            .expect("no answer error");
        let DigestStatus::Clarify(questions) = digested.status else {
            panic!("expected clarify");
        };
        let paths: Vec<&str> = questions.iter().map(|q| q.path.as_str()).collect();
        assert!(paths.contains(&"$.spec"), "paths: {paths:?}");
        assert!(paths.contains(&"$.tasks[0].run"), "paths: {paths:?}");
    }

    #[test]
    fn hedge_number_produces_bridged_candidates() {
        let digested = digest_model_output("test", "\"2 or 3\"", &Schema::num(), vec![])
            .expect("no answer error");
        let DigestStatus::Clarify(questions) = digested.status else {
            panic!("expected clarify");
        };
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].candidates, vec!["2", "3"]);
        let Event::ModelOutputDigested {
            questions: mirror, ..
        } = &digested.event
        else {
            panic!("wrong event kind");
        };
        assert_eq!(mirror[0].candidates, vec!["2", "3"]);
    }

    #[test]
    fn answers_apply_only_to_questioned_paths() {
        let raw = r#"{"tasks": []}"#;
        let answer = Answer::new("$.spec", PValue::Str("Recovered spec.".into()));
        let digested = digest_model_output("proposal", raw, &proposal_schema(), vec![answer])
            .expect("requested path");
        let DigestStatus::Resolved(value) = digested.status else {
            panic!("expected resolved after answering");
        };
        assert_eq!(
            value.get("spec").and_then(|v| v.as_str()),
            Some("Recovered spec.")
        );
        let Event::ModelOutputDigested { answers, .. } = &digested.event else {
            panic!("wrong event kind");
        };
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].path, "$.spec");

        let bogus = Answer::new("$.nonexistent", PValue::Bool(true));
        let err = digest_model_output("proposal", raw, &proposal_schema(), vec![bogus])
            .expect_err("unrequested path must be refused");
        assert!(err.to_string().contains("answer rejected"), "err: {err}");
    }

    #[test]
    fn review_envelope_case_folds_and_flags_ambiguity() {
        let raw = "```json\n{\"recommendation\": \"APPROVE\", \"body\": \"## Shared question\\nIs it safe?\"}\n```";
        let digested = digest_model_output("review_draft:evidence", raw, &review_schema(), vec![])
            .expect("no answer error");
        let DigestStatus::Resolved(value) = digested.status else {
            panic!("expected resolved");
        };
        assert!(repair_codes(&digested.repairs).contains(&"case_folded"));
        let (decision, body) = extract_review(&value).expect("extracts");
        assert_eq!(decision, ReviewDecision::Approve);
        assert!(body.starts_with("## Shared question"));

        let ambiguous = r#"{"recommendation": "maybe", "body": "text"}"#;
        let digested =
            digest_model_output("review_draft:evidence", ambiguous, &review_schema(), vec![])
                .expect("no answer error");
        let DigestStatus::Clarify(questions) = digested.status else {
            panic!("expected clarify");
        };
        assert_eq!(questions[0].path, "$.recommendation");
        assert_eq!(questions[0].candidates, vec!["approve", "reject"]);
    }

    #[test]
    fn empty_tasks_array_resolves_with_zero_tasks() {
        let raw = r#"{"spec": "s", "tasks": []}"#;
        let digested = digest_model_output("proposal", raw, &proposal_schema(), vec![])
            .expect("no answer error");
        let DigestStatus::Resolved(value) = digested.status else {
            panic!("expected resolved");
        };
        let (_, tasks) = extract_proposal(&value).expect("extracts");
        assert!(tasks.is_empty());
    }

    #[test]
    fn hopeless_text_is_unparseable_not_an_error() {
        let digested = digest_model_output(
            "proposal",
            "no json here at all",
            &proposal_schema(),
            vec![],
        )
        .expect("unparseable is not Err");
        assert!(matches!(digested.status, DigestStatus::Unparseable(_)));
        let Event::ModelOutputDigested { status, .. } = &digested.event else {
            panic!("wrong event kind");
        };
        assert_eq!(status, "unparseable");
    }
}
