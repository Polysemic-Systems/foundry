use anyhow::{Context, Result, bail};
use foundry_core::{DiscourseAct, DiscourseSpeaker, DiscourseTurn, Event, Graph, Plan};
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::commands::ask;
use crate::{SOCRATIC_DISCOURSE_CONTRACT, digest_boundary, lease, task_contract};

/// The proposal-mode output contract, enforced by the digest boundary.
const PROPOSAL_JSON_RULES: &str = "Rules:\n\
- Respond with exactly ONE JSON object and nothing else: no markdown fence, no prose before or after.\n\
- Shape: {\"spec\": \"<2-4 sentence summary>\", \"tasks\": [{\"description\": \"...\", \"files\": [\"path1\", \"path2\"], \"run\": \"command\"}]}\n\
- files: repository-relative paths present in the supplied context only. Prefix an intentionally created path with `new:`. Omit the files key entirely if unknown — never emit null.\n\
- run: only include if there is a clear safe command (e.g. `cargo test`, `just check`). Omit the key if not — never emit null.\n\
- Keep tasks small and concrete. Prefer 3-7 tasks. No numbering; array order is task order.\n\
- Escape newlines inside JSON strings as \\n.\n\
- Do NOT ask further questions.\n\
- Do NOT include meta-tasks like \"discuss with user\" or \"verify with user\".";

/// Ask the model for a fresh proposal after a boundary rejection or human
/// feedback, keeping the discourse transcript and event log accurate.
fn regenerate_proposal(
    graph: &mut Graph,
    messages: &mut Vec<(String, String)>,
    model: &str,
    discourse_key: &str,
    previous_turn: &DiscourseTurn,
    prior_output: &str,
    instruction: String,
) -> Result<(String, DiscourseTurn)> {
    messages.push(("assistant".to_string(), prior_output.to_string()));
    messages.push(("user".to_string(), instruction));
    let (new_proposal, prompt_tokens, _completion_tokens) =
        ask::chat_with_model(model, messages).context("regenerating feature proposal")?;
    graph.record_event(&Event::ModelInvoked {
        model: model.to_string(),
        prompt_tokens,
        cost_usd: 0.0,
    })?;
    let turn = DiscourseTurn::new(
        discourse_key,
        DiscourseSpeaker::SocraticPartner,
        DiscourseAct::Synthesis,
        new_proposal.clone(),
        Some(previous_turn.id),
    );
    graph.record_discourse_turn(&turn)?;
    Ok((new_proposal, turn))
}

pub fn cmd_propose(
    query: Option<&str>,
    plan_path: &Path,
    root: &Path,
    db: &Path,
    model: &str,
) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    // 1. Collect the feature description.
    let description = match query {
        Some(q) => q.trim().to_string(),
        None => {
            print!("Describe the feature: ");
            std::io::stdout().flush().context("flushing stdout")?;
            let mut buf = String::new();
            std::io::stdin()
                .read_line(&mut buf)
                .context("reading feature description from stdin")?;
            buf.trim().to_string()
        }
    };
    if description.is_empty() {
        bail!("feature description is empty");
    }
    let discourse_key = format!("proposal:{}", uuid::Uuid::new_v4());
    let initial_inquiry = DiscourseTurn::new(
        &discourse_key,
        DiscourseSpeaker::Human,
        DiscourseAct::Question,
        description.clone(),
        None,
    );
    graph.record_discourse_turn(&initial_inquiry)?;

    // 2. Gather relevant code context from the graph.
    let safe_query = foundry_core::sanitize_query(&description);
    let context_results = graph
        .search_code(&safe_query)
        .context("searching code for context")?;
    let mut context = String::new();
    for (i, (node, content)) in context_results.iter().take(5).enumerate() {
        context.push_str(&format!(
            "--- snippet {}: {} ---\n{}\n\n",
            i + 1,
            node.name,
            content.chars().take(1500).collect::<String>()
        ));
    }
    if context.is_empty() {
        context.push_str("(no indexed code context found)");
    }

    let system_prompt = format!(
        "You are Foundry, a Socratic engineering partner helping turn feature ideas into concrete implementation tasks for a Rust project.\n{}",
        SOCRATIC_DISCOURSE_CONTRACT
    );

    let mut messages: Vec<(String, String)> = vec![("system".to_string(), system_prompt)];

    // 3. First LLM turn: ask clarifying questions.
    messages.push((
        "user".to_string(),
        format!(
            "MODE: question-mode\n\nThe user wants to add this feature: \"{}\"\n\nRelevant codebase context:\n\n{}\n\nYour job: ask 2-4 focused Socratic questions. Each question must expose an assumption, tradeoff, evidence gap, or plausible competing interpretation whose answer would change the design.\nRules:\n- Ask ONLY decision-bearing questions.\n- Do NOT propose tasks, files, commands, or a spec.\n- Do NOT write code or implementation details.",
            description, context
        ),
    ));

    let (questions, prompt_tokens, _completion_tokens) =
        ask::chat_with_model(model, &messages).context("asking clarifying questions")?;
    graph
        .record_event(&Event::ModelInvoked {
            model: model.to_string(),
            prompt_tokens,
            cost_usd: 0.0,
        })
        .context("recording model invocation")?;
    let questions_turn = DiscourseTurn::new(
        &discourse_key,
        DiscourseSpeaker::SocraticPartner,
        DiscourseAct::Question,
        questions.clone(),
        Some(initial_inquiry.id),
    );
    graph.record_discourse_turn(&questions_turn)?;
    println!("\n{}\n", questions);

    // 4. Read the user's answers.
    println!("Your answers (submit an empty line when finished):");
    std::io::stdout().flush().context("flushing stdout")?;
    let mut answers = String::new();
    let stdin = std::io::stdin();
    loop {
        let mut line = String::new();
        stdin
            .read_line(&mut line)
            .context("reading answers from stdin")?;
        if line.trim().is_empty() {
            break;
        }
        answers.push_str(&line);
    }
    if answers.trim().is_empty() {
        bail!("no answers provided; aborting");
    }
    let answers_turn = DiscourseTurn::new(
        &discourse_key,
        DiscourseSpeaker::Human,
        DiscourseAct::Synthesis,
        answers.trim().to_owned(),
        Some(questions_turn.id),
    );
    graph.record_discourse_turn(&answers_turn)?;

    // Ollama can take several minutes on CPU-only machines. Acknowledge the
    // terminator before starting the blocking request so submitted input does
    // not look like a frozen terminal.
    println!(
        "Answers received ({} characters). Generating proposal with {}...",
        answers.trim().chars().count(),
        model
    );
    std::io::stdout().flush().context("flushing stdout")?;

    // 5. Second LLM turn: produce spec and tasks.
    messages.push(("assistant".to_string(), questions));
    messages.push((
        "user".to_string(),
        format!(
            "MODE: proposal-mode\n\nThe user answered:\n\n{}\n\nSynthesize the discourse into a concise feature spec and a list of concrete implementation tasks. Make important assumptions explicit in the spec and ensure each task contains or implies falsifying evidence.\n{}",
            answers, PROPOSAL_JSON_RULES
        ),
    ));

    let (mut proposal, prompt_tokens2, _completion_tokens2) =
        ask::chat_with_model(model, &messages).context("generating feature proposal")?;
    graph
        .record_event(&Event::ModelInvoked {
            model: model.to_string(),
            prompt_tokens: prompt_tokens2,
            cost_usd: 0.0,
        })
        .context("recording model invocation")?;
    let mut proposal_turn = DiscourseTurn::new(
        &discourse_key,
        DiscourseSpeaker::SocraticPartner,
        DiscourseAct::Synthesis,
        proposal.clone(),
        Some(answers_turn.id),
    );
    graph.record_discourse_turn(&proposal_turn)?;

    // 6. Confirm, edit, or abort. Every proposal crosses the digest boundary:
    // repairs are named on a ledger, genuine ambiguity becomes questions the
    // human can answer, and each crossing is a recorded event.
    let schema = digest_boundary::proposal_schema();
    let mut clarifications: Vec<polysemic_digest::Answer> = Vec::new();
    loop {
        let digested = match digest_boundary::digest_model_output(
            "proposal",
            &proposal,
            &schema,
            clarifications.clone(),
        ) {
            Ok(digested) => digested,
            Err(error) => {
                // A rejected clarification (duplicate path, bad input) is
                // re-promptable, not fatal.
                println!("{error:#}");
                clarifications.clear();
                continue;
            }
        };
        digest_boundary::print_repair_ledger("proposal", &digested.repairs);
        graph
            .record_event(&digested.event)
            .context("recording digest boundary event")?;

        let value = match digested.status {
            digest_boundary::DigestStatus::Resolved(value) => value,
            digest_boundary::DigestStatus::Unparseable(reason) => {
                let preview: String = proposal.chars().take(2000).collect();
                println!(
                    "\nThe model response did not survive the digest boundary ({}). Raw output (truncated):\n{}\n",
                    reason, preview
                );
                print!("[r=retry / n=abort]: ");
                std::io::stdout().flush()?;
                let mut choice = String::new();
                std::io::stdin().read_line(&mut choice)?;
                if !choice.trim().eq_ignore_ascii_case("r") {
                    println!("Aborted. No tasks added.");
                    return Ok(());
                }
                let instruction = format!(
                    "MODE: proposal-mode\n\nYour previous response was rejected by the JSON boundary: {}\nTry again.\n{}",
                    reason, PROPOSAL_JSON_RULES
                );
                let (new_proposal, turn) = regenerate_proposal(
                    &mut graph,
                    &mut messages,
                    model,
                    &discourse_key,
                    &proposal_turn,
                    &proposal,
                    instruction,
                )?;
                proposal = new_proposal;
                proposal_turn = turn;
                clarifications.clear();
                continue;
            }
            digest_boundary::DigestStatus::Clarify(questions) => {
                println!(
                    "\nThe proposal is ambiguous; the boundary raised questions instead of guessing:"
                );
                for question in &questions {
                    println!("  {question}");
                }
                print!("[a=answer questions / r=retry model / n=abort]: ");
                std::io::stdout().flush()?;
                let mut choice = String::new();
                std::io::stdin().read_line(&mut choice)?;
                match choice.trim().to_lowercase().as_str() {
                    "a" | "answer" => {
                        clarifications
                            .extend(digest_boundary::collect_answers_interactively(&questions)?);
                    }
                    "r" | "retry" => {
                        let listed = questions
                            .iter()
                            .map(|q| format!("- {q}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let instruction = format!(
                            "MODE: proposal-mode\n\nYour previous response left these fields ambiguous:\n{}\nTry again.\n{}",
                            listed, PROPOSAL_JSON_RULES
                        );
                        let (new_proposal, turn) = regenerate_proposal(
                            &mut graph,
                            &mut messages,
                            model,
                            &discourse_key,
                            &proposal_turn,
                            &proposal,
                            instruction,
                        )?;
                        proposal = new_proposal;
                        proposal_turn = turn;
                        clarifications.clear();
                    }
                    _ => {
                        println!("Aborted. No tasks added.");
                        return Ok(());
                    }
                }
                continue;
            }
        };

        let (spec, tasks) = digest_boundary::extract_proposal(&value)?;
        if tasks.is_empty() {
            println!("\nThe model proposed zero tasks.");
            print!("[r=retry / n=abort]: ");
            std::io::stdout().flush()?;
            let mut choice = String::new();
            std::io::stdin().read_line(&mut choice)?;
            if !choice.trim().eq_ignore_ascii_case("r") {
                println!("Aborted. No tasks added.");
                return Ok(());
            }
            let instruction = format!(
                "MODE: proposal-mode\n\nYour previous response contained zero tasks. Try again.\n{}",
                PROPOSAL_JSON_RULES
            );
            let (new_proposal, turn) = regenerate_proposal(
                &mut graph,
                &mut messages,
                model,
                &discourse_key,
                &proposal_turn,
                &proposal,
                instruction,
            )?;
            proposal = new_proposal;
            proposal_turn = turn;
            clarifications.clear();
            continue;
        }

        println!("\n=== Proposed Feature ===\n{}\n", spec);
        println!("=== Tasks to add to {} ===", plan_path.display());
        for (i, (desc, files, run)) in tasks.iter().enumerate() {
            let mut line = format!("{}. [ ] {}", i + 1, desc);
            if !files.is_empty() {
                line.push_str(&format!(" - files: {}", files.join(", ")));
            }
            if let Some(run_cmd) = run {
                line.push_str(&format!(" - run: {}", run_cmd));
            }
            println!("{}", line);
        }

        let validation_errors = tasks
            .iter()
            .flat_map(|(description, files, run)| {
                task_contract::validate(root, files, run.as_deref())
                    .into_iter()
                    .map(move |error| format!("{description}: {error}"))
            })
            .collect::<Vec<_>>();
        if !validation_errors.is_empty() {
            println!("\nProposal validation failed:");
            for error in &validation_errors {
                println!("  - {error}");
            }
            println!("Use `new:path` only when the task intentionally creates that file.");
        }

        print!("\nApprove? [y=append to plan / e=edit / n=abort]: ");
        std::io::stdout().flush()?;
        let mut choice = String::new();
        std::io::stdin().read_line(&mut choice)?;
        match choice.trim().to_lowercase().as_str() {
            "y" | "yes" => {
                if !validation_errors.is_empty() {
                    println!("Cannot append an ungrounded proposal; choose edit or abort.");
                    continue;
                }
                let mutation =
                    lease::acquire_repository(root, &lease::default_owner(), "propose approval")
                        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
                let plan_path = mutation.require_path(plan_path)?;
                if let Some(parent) = plan_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("creating plan directory {:?}", parent))?;
                }
                let plan_text = fs::read_to_string(&plan_path)
                    .unwrap_or_else(|_| "# Feature Backlog\n\n".to_string());
                let mut plan = Plan::parse_path(&plan_path, &plan_text);
                for (desc, files, run) in &tasks {
                    plan.append_task(desc, files, run.as_deref());
                }
                fs::write(&plan_path, plan.to_string())
                    .with_context(|| format!("writing plan to {:?}", plan_path))?;

                let appended_count = tasks.len();
                let task_ids: Vec<String> = plan
                    .tasks
                    .iter()
                    .rev()
                    .take(appended_count)
                    .rev()
                    .map(|t| t.id.to_string())
                    .collect();
                let plan_relative = plan_path
                    .strip_prefix(root)
                    .unwrap_or(&plan_path)
                    .to_string_lossy()
                    .to_string();
                graph
                    .record_event(&Event::FeatureProposed {
                        title: description.clone(),
                        plan_path: plan_relative,
                        task_ids,
                    })
                    .context("recording feature proposed event")?;
                println!(
                    "Added {} task(s) to {}.",
                    appended_count,
                    plan_path.display()
                );
                return Ok(());
            }
            "e" | "edit" => {
                println!("What should change? (end with a blank line):");
                std::io::stdout().flush()?;
                let mut feedback = String::new();
                loop {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    if line.trim().is_empty() {
                        break;
                    }
                    feedback.push_str(&line);
                }
                let feedback_turn = DiscourseTurn::new(
                    &discourse_key,
                    DiscourseSpeaker::Human,
                    DiscourseAct::Challenge,
                    feedback.trim().to_owned(),
                    Some(proposal_turn.id),
                );
                graph.record_discourse_turn(&feedback_turn)?;
                let instruction = format!(
                    "Please revise the proposal based on this feedback:\n\n{}\n{}",
                    feedback, PROPOSAL_JSON_RULES
                );
                let (new_proposal, turn) = regenerate_proposal(
                    &mut graph,
                    &mut messages,
                    model,
                    &discourse_key,
                    &feedback_turn,
                    &proposal,
                    instruction,
                )?;
                proposal = new_proposal;
                proposal_turn = turn;
                clarifications.clear();
                continue;
            }
            _ => {
                println!("Aborted. No tasks added.");
                return Ok(());
            }
        }
    }
}
