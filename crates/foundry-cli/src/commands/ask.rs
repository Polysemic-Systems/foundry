use anyhow::{Context, Result, bail};
use foundry_core::{DiscourseAct, DiscourseSpeaker, DiscourseTurn, Event, Graph, sanitize_query};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::SOCRATIC_DISCOURSE_CONTRACT;

struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Call a local Ollama model with a chat history.
/// Returns the assistant's reply plus token counts.
fn ask_ollama(model: &str, messages: &[ChatMessage<'_>]) -> Result<(String, u64, u64)> {
    let payload_messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
        .collect();
    let payload = serde_json::json!({
        "model": model,
        "messages": payload_messages,
        "stream": false,
    });

    let mut child = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "http://localhost:11434/api/chat",
            "-H",
            "Content-Type: application/json",
            "-d",
            "@-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning curl to call Ollama. Is Ollama running on localhost:11434?")?;

    {
        let mut stdin = child.stdin.take().context("opening curl stdin")?;
        stdin
            .write_all(payload.to_string().as_bytes())
            .context("writing prompt to curl")?;
    }

    let output = child.wait_with_output().context("waiting for curl")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("curl failed: {}", stderr);
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing Ollama response")?;
    if let Some(error) = json.get("error").and_then(|v| v.as_str()) {
        bail!("Ollama error: {}", error);
    }
    let answer = json
        .get("message")
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_str())
        .with_context(|| {
            format!(
                "Ollama response has no message.content: {}",
                String::from_utf8_lossy(&output.stdout)
                    .chars()
                    .take(500)
                    .collect::<String>()
            )
        })?
        .to_string();
    if answer.trim().is_empty() {
        bail!("Ollama returned an empty message.content");
    }
    let prompt_tokens = json
        .get("prompt_eval_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion_tokens = json.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0);

    Ok((answer, prompt_tokens, completion_tokens))
}

pub fn chat_with_model(model: &str, messages: &[(String, String)]) -> Result<(String, u64, u64)> {
    let chat_messages: Vec<ChatMessage<'_>> = messages
        .iter()
        .map(|(role, content)| ChatMessage {
            role: role.as_str(),
            content: content.as_str(),
        })
        .collect();
    ask_ollama(model, &chat_messages)
}

pub fn embed_ollama(model: &str, text: &str) -> Result<Vec<f32>> {
    let payload = serde_json::json!({
        "model": model,
        "prompt": text,
    });

    let mut child = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "http://localhost:11434/api/embeddings",
            "-H",
            "Content-Type: application/json",
            "-d",
            "@-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(
            "spawning curl to call Ollama embeddings. Is Ollama running on localhost:11434?",
        )?;

    {
        let mut stdin = child.stdin.take().context("opening curl stdin")?;
        stdin
            .write_all(payload.to_string().as_bytes())
            .context("writing prompt to curl")?;
    }

    let output = child.wait_with_output().context("waiting for curl")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("curl failed: {}", stderr);
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing Ollama embeddings response")?;
    if let Some(error) = json.get("error").and_then(|v| v.as_str()) {
        bail!("Ollama embeddings error: {}", error);
    }
    let embedding = json
        .get("embedding")
        .and_then(|v| v.as_array())
        .context("missing embedding array in response")?
        .iter()
        .map(|v| {
            v.as_f64()
                .map(|f| f as f32)
                .context("non-numeric embedding value")
        })
        .collect::<Result<Vec<_>, _>>()
        .context("embedding values")?;

    Ok(embedding)
}

pub fn cmd_ask(db: &Path, model: &str, query: &str, limit: usize) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    let discourse_key = format!("ask:{}", uuid::Uuid::new_v4());
    let inquiry = DiscourseTurn::new(
        &discourse_key,
        DiscourseSpeaker::Human,
        DiscourseAct::Question,
        query,
        None,
    );
    graph.record_discourse_turn(&inquiry)?;

    let safe_query = sanitize_query(query);
    let results = graph
        .search_code(&safe_query)
        .context("searching graph for context")?;

    if results.is_empty() {
        let observation = format!(
            "No indexed code matched '{safe_query}'. What more specific evidence should we examine?"
        );
        graph.record_discourse_turn(&DiscourseTurn::new(
            &discourse_key,
            DiscourseSpeaker::System,
            DiscourseAct::Observation,
            observation.clone(),
            Some(inquiry.id),
        ))?;
        println!("{observation}");
        return Ok(());
    }

    let mut context = String::new();
    for (i, (node, content)) in results.iter().take(limit).enumerate() {
        context.push_str(&format!(
            "--- snippet {}: {} ---\n{}\n\n",
            i + 1,
            node.name,
            content.chars().take(2000).collect::<String>()
        ));
    }

    let prompt = format!(
        "{}\n\nUse the following code snippets from the codebase as observed evidence in a \
         discourse with the user. Answer the shared question directly, identify assumptions and a plausible \
         competing interpretation, and end with one question only if its answer would materially change the \
         conclusion or next action.\n\n{}\nShared question: {}\nSocratic synthesis:",
        SOCRATIC_DISCOURSE_CONTRACT, context, query
    );

    let messages = vec![ChatMessage {
        role: "user",
        content: prompt.as_str(),
    }];
    let (answer, prompt_tokens, _completion_tokens) =
        ask_ollama(model, &messages).with_context(|| format!("asking model {}", model))?;

    graph
        .record_event(&Event::ModelInvoked {
            model: model.to_string(),
            prompt_tokens,
            cost_usd: 0.0,
        })
        .context("recording model invocation")?;
    graph.record_discourse_turn(&DiscourseTurn::new(
        &discourse_key,
        DiscourseSpeaker::SocraticPartner,
        DiscourseAct::Synthesis,
        answer.clone(),
        Some(inquiry.id),
    ))?;

    println!("{}", answer);
    Ok(())
}

pub fn cmd_semsearch(db: &Path, model: &str, query: &str, limit: usize) -> Result<()> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    let query_embedding =
        embed_ollama(model, query).with_context(|| format!("embedding query with {}", model))?;

    let results = graph
        .semantic_search(&query_embedding, limit)
        .context("running semantic search")?;

    if results.is_empty() {
        println!(
            "No embeddings found. Run `foundry index --embed` or `foundry rebuild --embed` first."
        );
        return Ok(());
    }

    for (node, score) in results {
        println!("{:.4}\t{}", score, node.name);
    }
    Ok(())
}
