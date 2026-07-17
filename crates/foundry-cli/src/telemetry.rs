use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

pub fn init() -> Result<()> {
    let filter = match std::env::var("FOUNDRY_LOG") {
        Ok(value) => EnvFilter::try_new(value).context("invalid FOUNDRY_LOG filter")?,
        Err(_) => EnvFilter::new("warn"),
    };
    let json =
        std::env::var("FOUNDRY_LOG_FORMAT").is_ok_and(|value| value.eq_ignore_ascii_case("json"));
    if json {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .json()
            .try_init()
            .map_err(|error| anyhow::anyhow!("initializing JSON tracing subscriber: {error}"))?;
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_target(false)
            .try_init()
            .map_err(|error| anyhow::anyhow!("initializing tracing subscriber: {error}"))?;
    }
    tracing::info!(
        format = if json { "json" } else { "text" },
        "initialized operational tracing"
    );
    Ok(())
}
