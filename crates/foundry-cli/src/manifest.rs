use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// Return the first sibling directory named by every Cargo path dependency
/// rooted at `../<sibling>/...`.
///
/// This deliberately parses TOML and visits dependency tables only: comments,
/// package metadata, and arbitrary strings containing `path =` are not
/// executable manifest dependencies.
pub fn sibling_path_dependencies(root: &Path) -> Result<Vec<String>> {
    let manifest = root.join("Cargo.toml");
    let text = match fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", manifest.display()));
        }
    };
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
    let mut paths = Vec::new();
    visit_tables(&value, &mut paths);

    let mut siblings = BTreeSet::new();
    for path in paths {
        let path = Path::new(path);
        let mut components = path.components();
        if components.next() != Some(Component::ParentDir) {
            continue;
        }
        let Some(Component::Normal(name)) = components.next() else {
            bail!(
                "path dependency {} is not rooted at one sibling directory",
                path.display()
            );
        };
        if components.any(|component| component == Component::ParentDir) {
            bail!(
                "path dependency {} escapes its sibling directory",
                path.display()
            );
        }
        siblings.insert(name.to_string_lossy().into_owned());
    }
    Ok(siblings.into_iter().collect())
}

fn visit_tables<'a>(value: &'a toml::Value, paths: &mut Vec<&'a str>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, value) in table {
        if DEPENDENCY_TABLES.contains(&key.as_str()) {
            collect_dependency_table(value, paths);
        } else {
            visit_tables(value, paths);
        }
    }
}

fn collect_dependency_table<'a>(value: &'a toml::Value, paths: &mut Vec<&'a str>) {
    let Some(dependencies) = value.as_table() else {
        return;
    };
    for dependency in dependencies.values() {
        if let Some(path) = dependency
            .as_table()
            .and_then(|details| details.get("path"))
            .and_then(toml::Value::as_str)
        {
            paths.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_ignores_comments_and_metadata_but_finds_nested_dependency_tables() {
        let root = std::env::temp_dir().join(format!("foundry-manifest-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "# path = \"../comment\"\n\
             [package]\nname = \"x\"\nversion = \"0.0.0\"\n\
             [package.metadata.fixture]\npath = \"../metadata\"\n\
             [workspace.dependencies]\none = { path = '../one/crate' }\n\
             [target.'cfg(unix)'.dev-dependencies]\ntwo = { path = \"../two\" }\n",
        )
        .unwrap();

        assert_eq!(
            sibling_path_dependencies(&root).unwrap(),
            vec!["one".to_string(), "two".to_string()]
        );
        fs::remove_dir_all(root).unwrap();
    }
}
