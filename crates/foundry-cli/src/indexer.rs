use crate::{
    commands::{ask::embed_ollama, plan::is_ignored},
    lease,
};
use anyhow::{Context, Result, bail};
use foundry_core::{graph::Graph, plan::Plan};
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

/// Character budget sent to the embedding model. Keep it below the model's
/// context length (nomic-embed-text supports about 2048 tokens).
const EMBED_MAX_CHARS: usize = 4000;

pub fn index(root: &Path, db: &Path, embed: bool) -> Result<()> {
    let mutation = lease::acquire_repository(root, &lease::default_owner(), "index")
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    index_with_mutation(&mutation, root, db, embed)
}

pub fn index_with_mutation(
    mutation: &lease::RepositoryMutation,
    root: &Path,
    db: &Path,
    embed: bool,
) -> Result<()> {
    require_root(mutation, root)?;
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {db:?}"))?;

    // Populate code search before plan linking so inferred dependencies see
    // every source file from one leased filesystem snapshot.
    let mut text_files = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !is_ignored(entry.path()))
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        text_files.push((relative, content));
    }

    let mut indexed = 0;
    let mut embedded = 0;
    let mut plans = Vec::new();
    let mut embed_available = embed;
    let mut embed_failure = None;
    let embed_model = "nomic-embed-text:latest";

    for (relative, content) in text_files {
        if relative.ends_with(".plan.md") {
            plans.push((relative, content));
            continue;
        }

        let embedding = if embed_available {
            let embed_text: String = content.chars().take(EMBED_MAX_CHARS).collect();
            match embed_ollama(embed_model, &embed_text) {
                Ok(embedding) => {
                    embedded += 1;
                    Some(embedding)
                }
                Err(error) => {
                    eprintln!(
                        "warning: embedding failed for {relative} ({error}); continuing without embeddings"
                    );
                    embed_available = false;
                    embed_failure = Some(format!("{relative}: {error}"));
                    None
                }
            }
        } else {
            None
        };

        if let Some(ref embedding) = embedding {
            graph
                .index_code_with_embedding(&relative, &content, Some(embedding))
                .with_context(|| format!("indexing {relative}"))?;
        } else {
            graph
                .index_code(&relative, &content)
                .with_context(|| format!("indexing {relative}"))?;
        }
        indexed += 1;
    }

    for (relative, content) in plans {
        let plan = Plan::parse_path(Path::new(&relative), &content);
        graph
            .index_plan(&relative, &plan)
            .with_context(|| format!("indexing plan {relative}"))?;
        indexed += 1;
    }

    if let Some(failure) = embed_failure {
        bail!(
            "Indexed {indexed} files, but embeddings are incomplete after {failure}. \
             Start Ollama and rerun `foundry index --embed`."
        );
    } else if embed {
        println!("Indexed {indexed} files ({embedded} with embeddings) into {db:?}");
    } else {
        println!("Indexed {indexed} files into {db:?}");
    }
    Ok(())
}

pub fn rebuild(root: &Path, db: &Path, embed: bool) -> Result<()> {
    let mutation = lease::acquire_repository(root, &lease::default_owner(), "rebuild")
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    rebuild_with_mutation(&mutation, root, db, embed)
}

pub fn rebuild_with_mutation(
    mutation: &lease::RepositoryMutation,
    root: &Path,
    db: &Path,
    embed: bool,
) -> Result<()> {
    require_root(mutation, root)?;
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {db:?}"))?;
    graph
        .truncate_derived()
        .context("truncating derived state")?;
    println!("Truncated derived state. Re-indexing...");
    index_with_mutation(mutation, root, db, embed)
}

fn require_root(mutation: &lease::RepositoryMutation, root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving repository root {}", root.display()))?;
    if root != mutation.root() {
        anyhow::bail!(
            "index root {} is outside leased repository {}",
            root.display(),
            mutation.root().display()
        );
    }
    Ok(())
}
