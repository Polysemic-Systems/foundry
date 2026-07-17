use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct PrivateScratch {
    path: PathBuf,
}

impl PrivateScratch {
    fn create(path: PathBuf) -> Result<Self> {
        if path.exists() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("removing stale agent scratch {}", path.display()))?;
        }
        create_private_dir(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateScratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn network_enabled() -> bool {
    std::env::var("FOUNDRY_AGENT_NETWORK").is_ok_and(|value| value.eq_ignore_ascii_case("on"))
}

pub fn run_editor(root: &Path, agent_command: &str, prompt: &str) -> Result<()> {
    let mut parts = parse_command(agent_command)?;
    let prompt_as_argument = matches!(parts.last().map(String::as_str), Some("--prompt" | "-p"));
    if prompt_as_argument {
        parts.push(prompt.to_owned());
    }
    let program = &parts[0];
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving isolated editor workspace {}", root.display()))?;
    let scratch = PrivateScratch::create(root.join(".foundry-agent-tmp"))?;
    let mut command = sandboxed_command(&root, scratch.path(), program, &parts[1..], true)?;
    let mut child = command
        .current_dir(&root)
        .stdin(if prompt_as_argument {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting isolated editor agent '{program}'"))?;
    if !prompt_as_argument {
        child
            .stdin
            .take()
            .context("opening editor agent stdin")?
            .write_all(prompt.as_bytes())
            .context("writing editor agent prompt")?;
    }
    let stdout = child
        .stdout
        .take()
        .context("capturing editor agent output")?;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("reading editor agent output")?;
        if let Some(command) = line.strip_prefix("To resume this session: ") {
            println!("Agent session: {command}");
        } else {
            println!("{line}");
        }
    }
    let status = child.wait().context("waiting for editor agent")?;
    if !status.success() {
        bail!("editor agent exited with status {status}");
    }
    Ok(())
}

pub fn run_reviewer(root: &Path, agent_command: &str, prompt: &str) -> Result<String> {
    let mut parts = parse_command(agent_command)?;
    let prompt_as_argument = matches!(parts.last().map(String::as_str), Some("--prompt" | "-p"));
    if prompt_as_argument {
        parts.push(prompt.into());
    }
    let program = &parts[0];
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving review workspace {}", root.display()))?;
    let scratch = PrivateScratch::create(std::env::temp_dir().join(format!(
        "foundry-review-agent-{}",
        uuid::Uuid::new_v4().simple()
    )))?;
    let mut command = sandboxed_command(&root, scratch.path(), program, &parts[1..], false)?;
    let mut child = command
        .current_dir(&root)
        .stdin(if prompt_as_argument {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting isolated review agent '{program}'"))?;
    if !prompt_as_argument {
        child
            .stdin
            .take()
            .context("opening review agent stdin")?
            .write_all(prompt.as_bytes())?;
    }
    let output = child
        .wait_with_output()
        .context("waiting for review agent")?;
    if !output.status.success() {
        bail!("review agent exited with status {}", output.status);
    }
    let body = String::from_utf8(output.stdout).context("review agent output is not UTF-8")?;
    if body.trim().is_empty() {
        bail!("review agent returned an empty draft");
    }
    Ok(body)
}

fn sandboxed_command(
    root: &Path,
    scratch: &Path,
    program: &str,
    args: &[String],
    writable_root: bool,
) -> Result<Command> {
    let invoked_name = Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(program);
    let network_enabled = network_enabled();
    require_network_consent(invoked_name, network_enabled)?;
    let sandbox_disabled =
        std::env::var("FOUNDRY_AGENT_SANDBOX").is_ok_and(|value| value.eq_ignore_ascii_case("off"));
    if sandbox_disabled {
        eprintln!(
            "warning: FOUNDRY_AGENT_SANDBOX=off; the agent inherits the host environment, HOME, process namespace, network, and unconstrained filesystem access"
        );
        let mut command = Command::new(program);
        command.args(args);
        return Ok(command);
    }
    if Command::new("bwrap").arg("--version").output().is_err() {
        bail!(
            "Bubblewrap is required for agent isolation; install `bwrap` or explicitly set FOUNDRY_AGENT_SANDBOX=off"
        );
    }
    let executable = resolve_executable(program)?;
    // Identity comes from the name the operator invoked, never the
    // canonicalized target: version-managed installs symlink `claude` to a
    // file literally named `2.1.211`, and matching on that silently skips
    // credential preparation and the network-consent gate.
    let clean_home = scratch.join("home");
    prepare_agent_home(invoked_name, &clean_home)?;
    if network_enabled {
        eprintln!(
            "warning: FOUNDRY_AGENT_NETWORK=on; the agent can reach host-network services and transmit visible workspace files and copied authentication material"
        );
    }
    let mut command = Command::new("bwrap");
    command.args(sandbox_arguments(
        root,
        scratch,
        &clean_home,
        &executable,
        writable_root,
        network_enabled,
    ));
    command.args(args);
    Ok(command)
}

fn require_network_consent(invoked_name: &str, enabled: bool) -> Result<()> {
    if matches!(invoked_name, "codex" | "kimi" | "claude") && !enabled {
        bail!(
            "remote editor agent `{invoked_name}` needs network access; explicitly consent with `export FOUNDRY_AGENT_NETWORK=on`, then retry",
        );
    }
    Ok(())
}

fn sandbox_arguments(
    root: &Path,
    scratch: &Path,
    clean_home: &Path,
    executable: &Path,
    writable_root: bool,
    network_enabled: bool,
) -> Vec<OsString> {
    let mut args = strings([
        "--die-with-parent",
        "--new-session",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
    ]);
    if !network_enabled {
        args.push("--unshare-net".into());
    }
    args.extend(strings(["--clearenv", "--tmpfs", "/"]));
    for runtime in ["/usr", "/bin", "/lib", "/lib64", "/etc"] {
        if Path::new(runtime).exists() {
            args.extend(["--ro-bind".into(), runtime.into(), runtime.into()]);
        }
    }
    args.extend(strings([
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--dir",
        "/foundry-bin",
    ]));
    args.extend([
        "--ro-bind".into(),
        executable.as_os_str().into(),
        "/foundry-bin/agent".into(),
    ]);
    if executable.file_name() == Some(OsStr::new("kimi"))
        && let Some(parent) = executable.parent()
    {
        for helper in ["rg", "fd"] {
            let source = parent.join(helper);
            if source.exists() {
                args.extend([
                    "--ro-bind".into(),
                    source.into_os_string(),
                    format!("/foundry-bin/{helper}").into(),
                ]);
            }
        }
    }
    args.push(if writable_root { "--bind" } else { "--ro-bind" }.into());
    args.extend([root.as_os_str().into(), root.as_os_str().into()]);
    args.extend([
        "--bind".into(),
        scratch.as_os_str().into(),
        scratch.as_os_str().into(),
        "--setenv".into(),
        "HOME".into(),
        clean_home.as_os_str().into(),
        "--setenv".into(),
        "TMPDIR".into(),
        scratch.as_os_str().into(),
        "--setenv".into(),
        "PATH".into(),
        "/foundry-bin:/usr/local/bin:/usr/bin:/bin".into(),
        "--setenv".into(),
        "USER".into(),
        "foundry-agent".into(),
        "--setenv".into(),
        "LOGNAME".into(),
        "foundry-agent".into(),
        "--setenv".into(),
        "LANG".into(),
        "C.UTF-8".into(),
        "--setenv".into(),
        "TERM".into(),
        std::env::var_os("TERM").unwrap_or_else(|| "xterm-256color".into()),
        "--chdir".into(),
        root.as_os_str().into(),
        "--".into(),
        "/foundry-bin/agent".into(),
    ]);
    args
}

fn strings<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(Into::into).collect()
}

fn resolve_executable(program: &str) -> Result<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return path
            .canonicalize()
            .with_context(|| format!("resolving agent executable {program}"));
    }
    let search = std::env::var_os("PATH").context("PATH is not set")?;
    for directory in std::env::split_paths(&search) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return candidate
                .canonicalize()
                .with_context(|| format!("resolving agent executable {program}"));
        }
    }
    bail!("agent executable not found on PATH: {program}")
}

fn prepare_agent_home(invoked_name: &str, clean_home: &Path) -> Result<()> {
    create_private_dir(clean_home)?;
    let host_home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(host_home) = host_home else {
        return Ok(());
    };
    match invoked_name {
        "codex" => {
            for relative in [
                ".codex/auth.json",
                ".codex/config.toml",
                ".codex/installation_id",
            ] {
                copy_private_config(&host_home, clean_home, relative)?;
            }
        }
        "kimi" => {
            for relative in [
                ".kimi-code/config.toml",
                ".kimi-code/tui.toml",
                ".kimi-code/device_id",
                ".kimi-code/credentials/kimi-code.json",
                ".kimi-code/oauth/kimi-code",
            ] {
                copy_private_config(&host_home, clean_home, relative)?;
            }
        }
        "claude" => {
            // Claude Code: OAuth credentials plus top-level config. Session
            // history, projects, and plugins stay on the host — the agent
            // sees only what authentication requires.
            for relative in [
                ".claude/.credentials.json",
                ".claude/settings.json",
                ".claude.json",
            ] {
                copy_private_config(&host_home, clean_home, relative)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn copy_private_config(host_home: &Path, clean_home: &Path, relative: &str) -> Result<()> {
    let source = host_home.join(relative);
    if !source.is_file() {
        return Ok(());
    }
    let destination = clean_home.join(relative);
    create_private_dir(destination.parent().context("agent config has no parent")?)?;
    fs::copy(&source, &destination)
        .with_context(|| format!("copying isolated agent configuration {}", source.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).with_context(
            || {
                format!(
                    "restricting copied agent configuration {}",
                    destination.display()
                )
            },
        )?;
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("creating private directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting private directory {}", path.display()))?;
    }
    Ok(())
}

fn parse_command(agent_command: &str) -> Result<Vec<String>> {
    if agent_command.trim().is_empty() || agent_command.chars().any(|c| ";|&<>$`\n\"'".contains(c))
    {
        bail!("agent command must be a simple executable and arguments without shell syntax");
    }
    Ok(agent_command
        .split_whitespace()
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parser_accepts_arguments_and_rejects_shell_syntax() {
        assert_eq!(
            parse_command("codex exec --full-auto -").unwrap(),
            ["codex", "exec", "--full-auto", "-"]
        );
        assert_eq!(
            parse_command("kimi --prompt").unwrap(),
            ["kimi", "--prompt"]
        );
        assert!(parse_command("codex exec -; rm -rf workspace").is_err());
        assert!(parse_command("").is_err());
    }

    #[test]
    fn sandbox_hides_host_root_and_disables_network_by_default() {
        let root = Path::new("/workspace");
        let scratch = Path::new("/scratch");
        let args = sandbox_arguments(
            root,
            scratch,
            Path::new("/scratch/home"),
            Path::new("/bin/true"),
            true,
            false,
        );
        let rendered = args
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(rendered.contains(&std::borrow::Cow::Borrowed("--unshare-net")));
        assert!(
            rendered.windows(3).all(|window| {
                !(window[0] == "--ro-bind" && window[1] == "/" && window[2] == "/")
            })
        );
        assert!(rendered.contains(&std::borrow::Cow::Borrowed("--clearenv")));
    }

    #[test]
    fn remote_agents_require_network_consent_before_sandbox_setup() {
        assert!(require_network_consent("codex", false).is_err());
        assert!(require_network_consent("kimi", false).is_err());
        assert!(require_network_consent("claude", false).is_err());
        assert!(require_network_consent("local-reviewer", false).is_ok());
        assert!(require_network_consent("codex", true).is_ok());
    }

    #[test]
    fn private_scratch_is_removed_on_every_return_path() {
        let path = std::env::temp_dir().join(format!(
            "foundry-private-scratch-{}",
            uuid::Uuid::new_v4().simple()
        ));
        {
            let scratch = PrivateScratch::create(path.clone()).unwrap();
            fs::write(scratch.path().join("credential"), "secret").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(scratch.path()).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
        assert!(!path.exists());
    }
}
