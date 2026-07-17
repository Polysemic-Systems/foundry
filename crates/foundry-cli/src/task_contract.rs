use std::path::{Component, Path};

pub fn validate(root: &Path, files: &[String], run: Option<&str>) -> Vec<String> {
    let mut errors = Vec::new();
    for file in files {
        let (creating, path) = file
            .strip_prefix("new:")
            .map_or((false, file.as_str()), |path| (true, path));
        let relative = Path::new(path);
        if path.is_empty()
            || relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            errors.push(format!("invalid workspace-relative file hint `{file}`"));
            continue;
        }
        let target = root.join(relative);
        if creating {
            if target.exists() {
                errors.push(format!("`{file}` is marked new but already exists"));
            }
        } else if !target.is_file() {
            errors.push(format!("referenced file does not exist: `{file}`"));
        }
    }
    if let Some(run) = run
        && let Err(error) = crate::commands::common::safe_job_command(run)
    {
        errors.push(format!(
            "unsafe or unsupported run command `{run}`: {error}"
        ));
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_grounded_paths_and_safe_commands() {
        let root = std::env::temp_dir().join(format!("foundry-plan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();

        assert!(
            validate(
                &root,
                &["src/lib.rs".into(), "new:src/new.rs".into()],
                Some("cargo test")
            )
            .is_empty()
        );
        assert_eq!(
            validate(&root, &["src/missing.rs".into()], Some("rm -rf /")).len(),
            2
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
