use std::collections::HashSet;

/// Sanitize a natural-language query for SQLite FTS5.
///
/// - Replaces non-alphanumeric characters with spaces.
/// - Removes common stop words.
/// - Joins remaining tokens with spaces for an implicit AND query.
///
/// If the query contains only stop words, all tokens are returned so the
/// caller can still attempt a search.
pub fn sanitize_query(query: &str) -> String {
    let mut normalized = String::new();
    for c in query.chars() {
        if c.is_alphanumeric() || c == '_' {
            normalized.push(c);
        } else {
            normalized.push(' ');
        }
    }

    let stop: HashSet<&str> = [
        "what", "does", "did", "do", "is", "are", "was", "were", "the", "a", "an", "how", "why",
        "where", "when", "who", "which", "can", "could", "would", "should", "will", "about",
        "this", "that", "these", "those", "in", "on", "at", "of", "for", "with", "to", "from",
        "and", "or", "not", "it", "its", "i", "you", "me", "my",
    ]
    .iter()
    .copied()
    .collect();

    let tokens: Vec<String> = normalized
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty() && !stop.contains(t.as_str()))
        .collect();

    if tokens.is_empty() {
        // Fallback: keep all tokens if the query was only stop words.
        normalized
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        tokens.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_query;

    #[test]
    fn removes_stop_words_and_punctuation() {
        assert_eq!(
            sanitize_query("What does EdgeIntegrityRule do?"),
            "edgeintegrityrule"
        );
    }

    #[test]
    fn splits_code_operators() {
        assert_eq!(sanitize_query("Graph::open usage"), "graph open usage");
    }

    #[test]
    fn falls_back_when_only_stop_words() {
        assert_eq!(sanitize_query("what is the"), "what is the");
    }
}
