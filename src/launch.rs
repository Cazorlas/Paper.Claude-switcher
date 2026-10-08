use crate::output::user_println;
use crate::signals::{ShutdownListener, ShutdownSignal};
use anyhow::{Context, Result};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TuiLaunchOutcome {
    Exited(i32),
    Shutdown {
        signal: ShutdownSignal,
        cleanup_error: Option<String>,
    },
}

async fn wait_for_child_or_shutdown(
    child: &mut std::process::Child,
    shutdown: &mut ShutdownListener,
) -> std::io::Result<Result<std::process::ExitStatus, ShutdownSignal>> {
    loop {
        tokio::select! {
            signal = shutdown.recv_signal() => return Ok(Err(signal)),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                if let Some(status) = child.try_wait()? {
                    return Ok(Ok(status));
                }
            }
        }
    }
}

/// Launch Claude Code for one alias from the TUI. Returns Claude Code's exit
/// code instead of terminating the paper-claude-switch process on failure.
pub(crate) async fn launch_for_tui(
    alias: &str,
    extra_args: Vec<String>,
    shutdown: &mut ShutdownListener,
) -> Result<TuiLaunchOutcome> {
    launch_interactive(Some(alias), extra_args, false, Some(shutdown)).await
}

pub(crate) async fn launch_cmd(
    alias: Option<&str>,
    args: Vec<String>,
    json: bool,
) -> Result<()> {
    finish_launch_cli(launch_interactive(alias, args, json, None).await?)
}

fn finish_launch_cli(outcome: TuiLaunchOutcome) -> Result<()> {
    let exit_code = match outcome {
        TuiLaunchOutcome::Exited(code) => code,
        TuiLaunchOutcome::Shutdown { signal, .. } => signal.exit_code(),
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// Switch to the alias (or the best account), then run Claude Code with the
/// passthrough args. The binary is resolved first, so a missing `claude`
/// leaves the live login alone.
async fn launch_interactive(
    alias: Option<&str>,
    args: Vec<String>,
    json: bool,
    tui_shutdown: Option<&mut ShutdownListener>,
) -> Result<TuiLaunchOutcome> {
    let claude = ensure_claude_available()?;

    let mut owned_shutdown;
    let interrupt = match tui_shutdown {
        Some(shutdown) => shutdown,
        None => {
            owned_shutdown = ShutdownListener::new().context("registering shutdown handlers")?;
            &mut owned_shutdown
        }
    };

    let target_alias = match alias {
        Some(alias) => alias.to_string(),
        None => crate::commands::profile::select_best_profile(json).await?.alias,
    };
    let outcome = tokio::task::spawn_blocking({
        let target_alias = target_alias.clone();
        move || crate::commands::profile::switch_alias(&target_alias)
    })
    .await
    .context("switch task panicked")??;
    crate::cache::set_last_used(&target_alias)?;
    if !json {
        let verb = match outcome {
            crate::claude_store::SwitchOutcome::Switched { .. } => "Switched to",
            crate::claude_store::SwitchOutcome::AlreadyActive => "Already using",
        };
        user_println(&format!("{verb}: {target_alias}"));
        user_println(&format!("Launching Claude Code with profile '{target_alias}'..."));
    }

    let mut child = spawn_claude(&claude, &args).context("Failed to start claude")?;
    let status = match wait_for_child_or_shutdown(&mut child, interrupt)
        .await
        .context("waiting for claude")?
    {
        Ok(status) => status,
        Err(signal) => {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(TuiLaunchOutcome::Shutdown { signal, cleanup_error: None });
        }
    };
    Ok(TuiLaunchOutcome::Exited(child_exit_code(&status)))
}

fn spawn_claude(
    command: &std::path::Path,
    args: &[String],
) -> std::io::Result<std::process::Child> {
    let is_script = cfg!(windows)
        && command
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
    let mut cmd = if is_script {
        let mut cmd = std::process::Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    } else {
        std::process::Command::new(command)
    };
    cmd.args(args).spawn()
}

/// `CS_CLAUDE_BIN` if set, else `claude` on PATH.
fn ensure_claude_available() -> Result<std::path::PathBuf> {
    if let Some(bin) = std::env::var_os("CS_CLAUDE_BIN").filter(|value| !value.is_empty()) {
        return Ok(bin.into());
    }
    command_on_path("claude").ok_or_else(|| {
        anyhow::anyhow!(
            "claude not found in PATH. Install: npm install -g @anthropic-ai/claude-code"
        )
    })
}

pub(crate) fn command_on_path(name: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let candidates = if cfg!(windows) {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.bat"),
            name.to_string(),
        ]
    } else {
        vec![name.to_string()]
    };
    for dir in std::env::split_paths(&paths) {
        for file in &candidates {
            let candidate = dir.join(file);
            if candidate.is_file() {
                return if candidate.is_absolute() {
                    Some(candidate)
                } else {
                    std::env::current_dir().ok().map(|cwd| cwd.join(candidate))
                };
            }
        }
    }
    None
}

/// The child's exit code, mapping a Unix signal death to `128 + signal`.
fn child_exit_code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(|s| 128 + s).unwrap_or(1)
        })
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}
