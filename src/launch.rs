use crate::output::{print_json, user_println};
use crate::signals::{ShutdownListener, ShutdownSignal};
use crate::{auth, config, profile};
use anyhow::{Context, Result};

/// How the window Codex needs to read the staged `auth.json` ended.
#[derive(Debug, PartialEq, Eq)]
enum LaunchWait {
    Elapsed,
    Interrupted(ShutdownSignal),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TuiLaunchOutcome {
    Exited(i32),
    Shutdown {
        signal: ShutdownSignal,
        cleanup_error: Option<String>,
    },
}

fn shutdown_outcome(signal: ShutdownSignal, cleanup: Result<()>) -> TuiLaunchOutcome {
    TuiLaunchOutcome::Shutdown {
        signal,
        cleanup_error: cleanup.err().map(|error| format!("{error:#}")),
    }
}

/// Waits out that window, returning early if the user interrupts.
///
/// `interrupt` is deliberately a parameter rather than something this function
/// builds: it has to be registered before staging starts. Tokio discards a
/// signal that arrives with nothing registered for it, so a listener created
/// here would leave the whole staging window under the default terminate
/// action — Ctrl+C during the swap would kill the process outright, with the
/// staged profile left live and the user's own credentials stranded in a
/// `.bak` file whose name nothing ever printed.
async fn wait_for_codex_to_read_auth(
    interrupt: &mut ShutdownListener,
    delay: std::time::Duration,
) -> LaunchWait {
    tokio::select! {
        _ = tokio::time::sleep(delay) => LaunchWait::Elapsed,
        signal = interrupt.recv_signal() => LaunchWait::Interrupted(signal),
    }
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

/// Launch Codex for one alias from the TUI. Returns Codex's exit code instead of
/// terminating the paper-claude-switch process on failure.
pub(crate) async fn launch_for_tui(
    alias: &str,
    model: Option<&str>,
    extra_args: Vec<String>,
    shutdown: &mut ShutdownListener,
) -> Result<TuiLaunchOutcome> {
    launch_interactive(
        Some(alias),
        extra_args,
        false,
        model,
        Some(shutdown),
    )
    .await
}

pub(crate) async fn launch_cmd(
    alias: Option<&str>,
    args: Vec<String>,
    json: bool,
    model: Option<&str>,
) -> Result<()> {
    finish_launch_cli(
        launch_interactive(
            alias,
            args,
            json,
            model,
            None,
        )
        .await?,
    )
}

fn finish_launch_cli(outcome: TuiLaunchOutcome) -> Result<()> {
    let exit_code = match outcome {
        TuiLaunchOutcome::Exited(code) => code,
        TuiLaunchOutcome::Shutdown {
            signal,
            cleanup_error,
        } => {
            if let Some(error) = cleanup_error {
                eprintln!("Error while cleaning up interrupted launch: {error}");
            }
            signal.exit_code()
        }
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

async fn launch_interactive(
    alias: Option<&str>,
    args: Vec<String>,
    json: bool,
    model: Option<&str>,
    tui_shutdown: Option<&mut ShutdownListener>,
) -> Result<TuiLaunchOutcome> {
    let codex_command = ensure_codex_available()?;
    auth::ensure_file_credentials_store()?;

    let forwarded = chatgpt_codex_argv(model, args);

    let target_alias = match alias {
        Some(alias) => {
            let profiles = profile::list_profiles()?;
            if !profiles.iter().any(|profile| profile == alias) {
                anyhow::bail!("profile '{}' not found", alias);
            }
            alias.to_string()
        }
        None => crate::commands::profile::select_best_profile(json).await?.alias,
    };

    let forwarded = if codex_argv_selects_server(&forwarded) {
        forwarded
    } else {
        embedded_codex_argv(codex_supports_no_daemon(&codex_command)?, forwarded)
    };

    let codex_auth = auth::codex_auth_path()?;
    // Unique per-invocation backup name (PID + timestamp): prevents two
    // concurrent `launch` commands from clobbering each other's backup.
    let backup = codex_auth.with_extension(format!(
        "json.bak.{}.{}",
        std::process::id(),
        auth::now_unix_secs()
    ));

    // Registered before the first byte of the user's auth.json moves, so a
    // SIGINT or SIGTERM anywhere from here to the restore is recorded rather than
    // discarded. See `wait_for_codex_to_read_auth`.
    let mut owned_shutdown;
    let interrupt = match tui_shutdown {
        Some(shutdown) => shutdown,
        None => {
            owned_shutdown = ShutdownListener::new()
                .context("registering shutdown handlers that guard the staged auth.json")?;
            &mut owned_shutdown
        }
    };

    // The dedicated launch lease covers only stage -> process start -> short
    // read window -> restore. It does not hold the auth write lock or wait for
    // the interactive child to exit.
    let launch_lease = tokio::task::spawn_blocking(profile::lock_launch_session)
        .await
        .context("launch lease task panicked")?
        .context("acquiring launch session lease")?;
    // All paper-claude-switch writers acquire this lease before mutating live auth,
    // so the existence snapshot cannot race a concurrent switch.
    let had_original = codex_auth.exists();

    // Swap auth.json → start codex → wait for it to read auth → restore.
    // Codex CLI reads auth.json only at startup, so we only need to hold
    // the swapped state for a few seconds, not the entire session.
    let stage_result = {
        let codex_auth2 = codex_auth.clone();
        let backup2 = backup.clone();
        let target_alias2 = target_alias.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _lock = profile::lock_live_auth().context("acquiring auth lock")?;

            if had_original {
                backup_launch_auth(&codex_auth2, &backup2)?;
            }

            profile::stage_profile_auth(&target_alias2)?;
            Ok(())
        })
        .await
        .context("lock task panicked")?
    };
    if let Err(stage_err) = stage_result {
        if backup.exists() || !had_original {
            let codex_auth2 = codex_auth.clone();
            let backup2 = backup.clone();
            let alias2 = target_alias.clone();
            tokio::task::spawn_blocking(move || {
                restore_launch_auth(&codex_auth2, &backup2, had_original, &alias2)
            })
            .await
            .context("restore task panicked after launch staging failure")??;
        }
        drop(launch_lease);
        return Err(stage_err).context("staging launch auth");
    }
    // The auth lock is released here; the launch lease keeps other live-auth
    // writers out until the staged file is restored.

    if !json {
        user_println(&format!("Launching Codex with profile '{target_alias}'..."));
    }

    let child_result = spawn_codex(&codex_command, &forwarded, None, json, None);

    let mut child = match child_result {
        Ok(child) => child,
        Err(spawn_err) => {
            let codex_auth2 = codex_auth.clone();
            let backup2 = backup.clone();
            let alias2 = target_alias.clone();
            tokio::task::spawn_blocking(move || {
                restore_launch_auth(&codex_auth2, &backup2, had_original, &alias2)
            })
            .await
            .context("restore task panicked after Codex spawn failure")??;
            drop(launch_lease);
            return Err(spawn_err).context("Failed to start codex");
        }
    };
    let pipes = take_codex_pipes(&mut child, json);

    // Give codex time to read auth.json, then restore immediately.
    // Configurable via [launch] restore_delay_secs (default: 3).
    let delay = std::time::Duration::from_secs(config::get().launch.restore_delay_secs);
    // An interrupt anywhere since staging began — including one that landed
    // while the swap itself was running — lands here, and the restore below
    // still runs.
    let shutdown = match wait_for_codex_to_read_auth(interrupt, delay).await {
        LaunchWait::Elapsed => None,
        LaunchWait::Interrupted(signal) => {
            if !json {
                user_println("Interrupted; restoring original auth.json...");
            }
            Some(signal)
        }
    };

    if let Some(signal) = shutdown {
        terminate_child(&mut child, pipes);
        let restore_result = {
            let codex_auth2 = codex_auth.clone();
            let backup2 = backup.clone();
            let alias2 = target_alias.clone();
            tokio::task::spawn_blocking(move || {
                restore_launch_auth(&codex_auth2, &backup2, had_original, &alias2)
            })
            .await
            .context("lock task panicked")?
        };
        drop(launch_lease);
        return Ok(shutdown_outcome(signal, restore_result));
    }

    let restore_result = {
        let codex_auth2 = codex_auth.clone();
        let backup2 = backup.clone();
        let alias2 = target_alias.clone();
        tokio::task::spawn_blocking(move || {
            restore_launch_auth(&codex_auth2, &backup2, had_original, &alias2)
        })
        .await
        .context("lock task panicked")?
    };
    drop(launch_lease);
    if let Err(error) = restore_result {
        terminate_child(&mut child, pipes);
        return Err(error);
    }

    // Tokio's Unix signal handler remains installed for the process lifetime,
    // so keep consuming shutdown signals after the restore instead of leaving
    // a later `kill` swallowed while Codex is still running.
    let status = match wait_for_child_or_shutdown(&mut child, interrupt).await.context("waiting for codex")? {
        Ok(status) => status,
        Err(signal) => {
            terminate_child(&mut child, pipes);
            return Ok(shutdown_outcome(signal, Ok(())));
        }
    };
    let captured = join_codex_pipes(pipes);

    let exit_code = child_exit_code(&status);

    if json {
        let mut payload = serde_json::json!({
            "ok": status.success(),
            "alias": target_alias,
            "action": "launched",
            "exit_code": exit_code,
            "codex_stdout": captured.stdout,
            "codex_stderr": captured.stderr,
            "codex_stdout_truncated": captured.stdout_truncated,
            "codex_stderr_truncated": captured.stderr_truncated,
        });
        if let Some(model) = display_model(model, &forwarded) {
            payload["model"] = serde_json::Value::String(model);
        }

        print_json(&payload);
    } else {
        user_println("codex exited");
    }

    Ok(TuiLaunchOutcome::Exited(exit_code))
}

/// Codex argv for a ChatGPT `launch`: optional `--model` and one-shot
/// reasoning are spliced after a Codex subcommand in `passthrough` (Codex
/// 0.149 ignores flags in front of `exec`). Interactive launch has no
/// subcommand, so those flags stay in front.
pub(crate) fn chatgpt_codex_argv(
    model: Option<&str>,
    passthrough: Vec<String>,
) -> Vec<String> {
    let mut extra = Vec::new();
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        extra.push("--model".to_string());
        extra.push(model.to_string());
    }

    splice_after_subcommand(extra, passthrough)
}

/// Keep a launched ChatGPT session on the staged `auth.json`.
///
/// Codex 0.157 and newer attaches an interactive session to the shared
/// app-server daemon, which keeps the account it loaded when it started, so
/// the staged credentials would never be read. `--no-daemon` runs the session
/// in process instead. It is a root option, so it goes before any subcommand.
/// An argv that already picks its server (`--no-daemon`, `--remote`, or the
/// daemon-only `agents` command) is left alone.
pub(crate) fn embedded_codex_argv(supports_no_daemon: bool, mut argv: Vec<String>) -> Vec<String> {
    if supports_no_daemon && !codex_argv_selects_server(&argv) {
        argv.insert(0, "--no-daemon".to_string());
    }
    argv
}

fn codex_argv_selects_server(argv: &[String]) -> bool {
    codex_syntax_indices(argv).into_iter().any(|index| {
        let arg = argv[index].as_str();
        arg == "--no-daemon" || arg == "--remote" || arg.starts_with("--remote=")
    }) || codex_subcommand_index(argv).is_some_and(|index| argv[index] == "agents")
}

/// `--no-daemon` exists since Codex 0.156; an older Codex rejects unknown
/// options, so its root help decides whether the flag can be passed.
fn codex_supports_no_daemon(command: &std::path::Path) -> Result<bool> {
    let mut probe = std::process::Command::new(command);
    probe
        .arg("--help")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Cold-starting a CLI wrapper can exceed two seconds on a busy machine.
    // This is a required routing decision, unlike the optional version probe.
    let output = crate::process::output_with_timeout(probe, std::time::Duration::from_secs(10))
        .context(
            "could not determine Codex account routing from --help; refusing to stage credentials",
        )?;
    no_daemon_support_from_help(output.status.success(), &output.stdout)
}

fn no_daemon_support_from_help(success: bool, stdout: &[u8]) -> Result<bool> {
    let help = String::from_utf8_lossy(stdout);
    if !success || !help.contains("Usage:") {
        anyhow::bail!(
            "Codex --help did not return usable help; refusing to stage credentials because account routing is unknown"
        );
    }
    Ok(help.contains("--no-daemon"))
}



fn splice_after_subcommand(overrides: Vec<String>, passthrough: Vec<String>) -> Vec<String> {
    let Some(idx) = codex_subcommand_index(&passthrough) else {
        let mut argv = overrides;
        argv.extend(passthrough);
        return argv;
    };
    // Codex 0.149 ignores options in front of the subcommand, so flags that
    // the user put before `exec` move after it along with our `-c` overrides.
    let mut argv = Vec::with_capacity(overrides.len() + passthrough.len());
    argv.push(passthrough[idx].clone());
    argv.extend(overrides);
    argv.extend(
        passthrough
            .into_iter()
            .enumerate()
            .filter_map(|(i, arg)| (i != idx).then_some(arg)),
    );
    argv
}

/// Exclude option values and everything after `--` before recognizing syntax.
/// A model, directory or prompt named `resume`/`agents` is not a subcommand.
fn codex_syntax_indices(args: &[String]) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut skip_next = false;
    let mut images = false;
    for (index, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            break;
        }
        if images && !arg.starts_with('-') {
            continue;
        }
        images = matches!(arg.as_str(), "-i" | "--image");
        indices.push(index);
        skip_next = !arg.contains('=')
            && (matches!(arg.as_str(),
                "--model" | "-m" | "--config" | "-c" | "--profile" | "-p"
                | "--sandbox" | "-s" | "--ask-for-approval" | "-a" | "--cd" | "-C"
                | "--image" | "-i" | "--remote" | "--enable" | "--disable"
                | "--add-dir" | "--local-provider"
            )
                || matches!(
                    arg.as_str(),
                    "--color" | "--output-schema" | "--output-last-message" | "-o"
                ));
    }
    indices
}

fn codex_subcommand_index(args: &[String]) -> Option<usize> {
    let index = codex_syntax_indices(args)
        .into_iter()
        .find(|&index| !args[index].starts_with('-'))?;
    crate::cli::is_codex_subcommand(&args[index]).then_some(index)
}

fn passthrough_model_value(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return None;
        }
        if arg == "--model" || arg == "-m" {
            return args.get(i + 1).cloned();
        }
        if let Some(value) = arg.strip_prefix("--model=") {
            return Some(value.to_string());
        }
        i += 1;
    }
    None
}

fn display_model(cs_model: Option<&str>, passthrough: &[String]) -> Option<String> {
    passthrough_model_value(passthrough).or_else(|| {
        cs_model
            .filter(|model| !model.is_empty())
            .map(str::to_string)
    })
}





fn spawn_codex(
    command: &std::path::Path,
    args: &[String],
    extra_env: Option<(String, String)>,
    json: bool,
    isolated_codex_home: Option<&std::path::Path>,
) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(command);
    cmd.args(args);
    if json {
        // `--json launch` is non-interactive: inherited stdin is often a pipe
        // (not a TTY), and Codex exec then waits to append it as extra input.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
    }
    if let Some((name, value)) = extra_env {
        cmd.env(name, value);
    }
    if let Some(home) = isolated_codex_home {
        cmd.env("CODEX_HOME", home);
    }
    cmd.spawn()
}





struct CodexPipes {
    stdout: Option<std::thread::JoinHandle<CapturedBytes>>,
    stderr: Option<std::thread::JoinHandle<CapturedBytes>>,
}

fn terminate_child(child: &mut std::process::Child, pipes: CodexPipes) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = join_codex_pipes(pipes);
}



struct CapturedCodexIo {
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

struct CapturedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

const CODEX_CAPTURE_LIMIT: usize = 1024 * 1024;

fn read_bounded(mut pipe: impl std::io::Read) -> CapturedBytes {
    let mut bytes = Vec::with_capacity(CODEX_CAPTURE_LIMIT.min(64 * 1024));
    let mut chunk = [0_u8; 16 * 1024];
    let mut truncated = false;
    while let Ok(read) = pipe.read(&mut chunk) {
        if read == 0 {
            break;
        }
        let remaining = CODEX_CAPTURE_LIMIT.saturating_sub(bytes.len());
        let kept = remaining.min(read);
        bytes.extend_from_slice(&chunk[..kept]);
        truncated |= kept < read;
    }
    CapturedBytes { bytes, truncated }
}

fn take_codex_pipes(child: &mut std::process::Child, json: bool) -> CodexPipes {
    if !json {
        return CodexPipes {
            stdout: None,
            stderr: None,
        };
    }
    CodexPipes {
        stdout: child
            .stdout
            .take()
            .map(|mut pipe| std::thread::spawn(move || read_bounded(&mut pipe))),
        stderr: child
            .stderr
            .take()
            .map(|mut pipe| std::thread::spawn(move || read_bounded(&mut pipe))),
    }
}

fn join_codex_pipes(pipes: CodexPipes) -> CapturedCodexIo {
    fn into_string(handle: Option<std::thread::JoinHandle<CapturedBytes>>) -> (String, bool) {
        let captured = handle.and_then(|h| h.join().ok()).unwrap_or(CapturedBytes {
            bytes: Vec::new(),
            truncated: false,
        });
        let mut text = String::from_utf8_lossy(&captured.bytes).into_owned();
        let mut truncated = captured.truncated;
        if text.len() > CODEX_CAPTURE_LIMIT {
            let mut end = CODEX_CAPTURE_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            truncated = true;
        }
        (text, truncated)
    }
    let (stdout, stdout_truncated) = into_string(pipes.stdout);
    let (stderr, stderr_truncated) = into_string(pipes.stderr);
    CapturedCodexIo {
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    }
}

/// Resolve the Codex command on PATH without running it: `codex --version`
/// writes PATH-alias helpers into `$CODEX_HOME/tmp`. The concrete path is
/// passed to the later spawn so preflight and execution use the same candidate.
fn ensure_codex_available() -> Result<std::path::PathBuf> {
    command_on_path("codex").ok_or_else(|| {
        anyhow::anyhow!("codex not found in PATH. Install: npm install -g @openai/codex")
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

/// Codex's exit code, mapping a Unix signal death to `128 + signal`.
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

fn backup_launch_auth(codex_auth: &std::path::Path, backup: &std::path::Path) -> Result<()> {
    let original = std::fs::read(codex_auth)
        .with_context(|| format!("reading {} for backup", codex_auth.display()))?;
    auth::atomic_write_private(backup, &original)
        .with_context(|| format!("backing up {}", codex_auth.display()))
}

/// Roll the staged profile back out of the live auth.json, keeping anything
/// Codex refreshed while it was staged.
///
/// `alias` is the profile that was staged, i.e. the owner of whatever Codex may
/// have rewritten in place.
fn restore_launch_auth(
    codex_auth: &std::path::Path,
    backup: &std::path::Path,
    had_original: bool,
    alias: &str,
) -> Result<()> {
    let _lock = profile::lock_live_auth().context("acquiring auth lock for restore")?;
    // Capture this before `preserve_refreshed_launch_auth` updates the profile.
    // A same-account backup with a different refresh token is a distinct live
    // credential and must be restored even when the staged account refreshed.
    let backup_matches_staged = if had_original && backup.exists() {
        let staged_path = profile::profile_auth_path(alias)?;
        match (std::fs::read(staged_path), std::fs::read(backup)) {
            (Ok(staged), Ok(original)) => launch_credentials_match(&staged, &original, alias),
            _ => false,
        }
    } else {
        false
    };
    let refreshed_live_preserved = match preserve_refreshed_launch_auth(codex_auth, alias) {
        Ok(true) => {
            user_println(&format!(
                "Codex refreshed the credentials of profile '{alias}'; saved them before restoring."
            ));
            true
        }
        Ok(false) => false,
        // An error here means the live file holds credentials newer than the
        // profile's that could not be stored: either they belong to another
        // account, or the write failed. Rolling back would overwrite — or with
        // no original, delete — the only copy the auth server still accepts,
        // and rotation makes that irreversible. Leaving the live file in place
        // is the recoverable outcome: `paper-claude-switch use` fixes a wrong account,
        // nothing fixes a destroyed token.
        Err(err) => {
            return Err(err).with_context(|| {
                let recovery = if had_original {
                    format!(
                        "The pre-launch auth.json is kept at {}, so nothing is lost: save the \
                         live credentials with `paper-claude-switch import {}`, then restore that \
                         backup by hand.",
                        backup.display(),
                        codex_auth.display()
                    )
                } else {
                    format!(
                        "There was no pre-launch auth.json, so deleting this file would lose \
                         these credentials outright: save them with `paper-claude-switch import {}`.",
                        codex_auth.display()
                    )
                };
                format!(
                    "refusing to roll back {}: it holds newer credentials that could not be \
                     saved into profile '{alias}'. {recovery}",
                    codex_auth.display()
                )
            });
        }
    };
    if had_original && refreshed_live_preserved && backup_matches_staged {
        let live = auth::read_auth(codex_auth).with_context(|| {
            format!(
                "reading live auth.json {} before deciding whether to restore backup",
                codex_auth.display()
            )
        })?;
        let original = auth::read_auth(backup).with_context(|| {
            format!(
                "reading launch auth backup {} before deciding whether to restore it",
                backup.display()
            )
        })?;
        if ensure_same_account(alias, &live, &original).is_ok() {
            std::fs::remove_file(backup)
                .with_context(|| format!("removing launch auth backup {}", backup.display()))?;
            return Ok(());
        }
    }
    if had_original {
        let saved = std::fs::read(backup)
            .with_context(|| format!("reading launch auth backup {}", backup.display()))?;
        auth::atomic_write_private(codex_auth, &saved).with_context(|| {
            format!(
                "restoring launch auth backup {} -> {}",
                backup.display(),
                codex_auth.display()
            )
        })?;
        std::fs::remove_file(backup)
            .with_context(|| format!("removing launch auth backup {}", backup.display()))?;
    } else if codex_auth.exists() {
        std::fs::remove_file(codex_auth)
            .with_context(|| format!("removing staged launch auth {}", codex_auth.display()))?;
    }
    Ok(())
}

fn launch_credentials_match(staged: &[u8], backup: &[u8], alias: &str) -> bool {
    let staged_value = serde_json::from_slice::<serde_json::Value>(staged).ok();
    let backup_value = serde_json::from_slice::<serde_json::Value>(backup).ok();
    let (Some(staged_value), Some(backup_value)) = (staged_value, backup_value) else {
        return staged == backup;
    };
    let (_, staged_refresh) = auth::extract_tokens(&staged_value);
    let (_, backup_refresh) = auth::extract_tokens(&backup_value);
    staged_refresh.is_some()
        && staged_refresh == backup_refresh
        && ensure_same_account(alias, &staged_value, &backup_value).is_ok()
}

/// Fold credentials Codex refreshed in place back into the staged profile.
///
/// Codex CLI refreshes on startup when the staged `last_refresh` is old enough,
/// and OpenAI rotates `refresh_token` on every use: the moment Codex refreshes,
/// the copy still stored in the profile is revoked. Restoring the backup over
/// that write would leave the profile holding a dead token — unrecoverable
/// without a full re-login, and undetectable until the profile is next used.
///
/// Returns whether the profile was updated. Nothing is written unless the live
/// file proves it is both newer than the profile and the same account, so a
/// stale or foreign live copy can never overwrite good credentials.
///
/// Caller MUST hold the lock from `lock_live_auth()`.
fn preserve_refreshed_launch_auth(codex_auth: &std::path::Path, alias: &str) -> Result<bool> {
    if !codex_auth.exists() {
        return Ok(false);
    }
    let profile_path = profile::profile_auth_path(alias)?;
    if !profile_path.exists() {
        return Ok(false);
    }
    let saved = auth::read_auth(&profile_path)
        .with_context(|| format!("reading profile '{alias}' auth.json"))?;
    let live = auth::read_auth(codex_auth).with_context(|| {
        format!(
            "reading live auth.json {} before launch restore",
            codex_auth.display()
        )
    })?;
    if !live_is_newer(&saved, &live) {
        return Ok(false);
    }
    ensure_same_account(alias, &saved, &live)?;
    // Managed Codex policy may change while the launched process is running.
    // Re-evaluate at the final credential-write boundary, after identity
    // checks but before the rotated token reaches the profile store.
    auth::validate_managed_auth_value(&live)?;
    auth::write_auth(&profile_path, &live)
        .with_context(|| format!("saving refreshed credentials into profile '{alias}'"))?;
    Ok(true)
}

/// `last_refresh` is the same RFC3339 stamp Codex and paper-claude-switch both write,
/// so a strictly later value is the evidence that Codex rotated the tokens.
/// A profile without a stamp loses to any live file that has one, because the
/// staged copy came from that profile and therefore had no stamp either.
fn live_is_newer(saved: &serde_json::Value, live: &serde_json::Value) -> bool {
    let Some(live_ts) = last_refresh(live) else {
        return false;
    };
    match last_refresh(saved) {
        Some(saved_ts) => live_ts > saved_ts,
        None => true,
    }
}

fn last_refresh(val: &serde_json::Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(val.get("last_refresh")?.as_str()?).ok()
}

/// Same rule as `profile::update_profile_from_live`: the email must be present
/// on both sides and equal, and account ids must agree when both are known.
fn ensure_same_account(
    alias: &str,
    saved: &serde_json::Value,
    live: &serde_json::Value,
) -> Result<()> {
    let saved = profile::extract_identity(saved);
    let live = profile::extract_identity(live);
    let email_matches = matches!(
        (&saved.email, &live.email),
        (Some(saved), Some(live)) if saved == live
    );
    let account_matches = match (&saved.account_id, &live.account_id) {
        (Some(saved), Some(live)) => saved == live,
        _ => true,
    };
    if email_matches && account_matches {
        return Ok(());
    }
    anyhow::bail!(
        "live auth.json was refreshed into a different account than profile '{alias}'; \
         leaving the profile untouched"
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    use super::*;
    #[test]
    fn ensure_codex_available_fails_when_codex_is_not_on_path() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let empty = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", empty.path());
        }
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("PATH", value),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _restore = Restore(previous);
        let err = ensure_codex_available().unwrap_err().to_string();
        assert!(
            err.contains("codex not found in PATH"),
            "unexpected error: {err}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn codex_cmd_found_by_preflight_is_used_for_spawn() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("codex.cmd"), "@echo off\r\nexit /b 0\r\n").unwrap();

        let previous = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", dir.path());
        }
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("PATH", value),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _restore = Restore(previous);

        let command = ensure_codex_available().expect("codex.cmd must satisfy the PATH preflight");
        let mut child = spawn_codex(&command, &[], None, true, None)
            .expect("the command accepted by preflight must also be spawnable");
        assert!(child.wait().unwrap().success());
    }



    #[test]
    fn passthrough_model_value_reads_long_short_and_equals_forms() {
        assert_eq!(
            passthrough_model_value(&["exec".into(), "--model".into(), "one-shot".into()]),
            Some("one-shot".into())
        );
        assert_eq!(
            passthrough_model_value(&["-m".into(), "one-shot".into(), "exec".into()]),
            Some("one-shot".into())
        );
        assert_eq!(
            passthrough_model_value(&["--model=one-shot".into(), "exec".into()]),
            Some("one-shot".into())
        );
        assert_eq!(
            passthrough_model_value(&["--".into(), "--model".into(), "not-a-flag".into()]),
            None
        );
    }











    /// Staging moves the user's live `auth.json` aside and puts a profile's
    /// credentials in its place; the restore that undoes it only runs once the
    /// wait below returns. A Ctrl+C pressed *during* staging therefore lands
    /// before the wait starts polling — and if the listener is created inside
    /// the wait, tokio has nothing registered at broadcast time, discards the
    /// signal's record of itself, and the default terminate action kills the
    /// process with the staged profile still live and the original stranded in
    /// a `.bak` file the user never sees named.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_interrupt_arriving_during_staging_still_triggers_the_restore() {
        use super::{LaunchWait, wait_for_codex_to_read_auth};
        use crate::signals::{RAISE_LOCK, ShutdownListener, ShutdownSignal};
        use std::time::Duration;
        use tokio::signal::unix::{SignalKind, signal};

        let _raise = RAISE_LOCK.lock().await;
        // Registered where `launch_cmd` registers it: before the first byte of
        // the user's auth.json is touched.
        let mut interrupt = ShutdownListener::new().expect("shutdown listener");

        // Turns "tokio finished broadcasting" into an awaitable event so the
        // assertion never depends on sleeping long enough.
        let mut witness = signal(SignalKind::interrupt()).expect("witness listener");

        // SAFETY: raising SIGINT at our own process, with both listeners above
        // already registered, so the default terminate action cannot fire.
        // This stands in for the staging window: the signal lands well before
        // anything polls for it.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        witness.recv().await;

        // A delay long enough that returning `Elapsed` is impossible.
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_codex_to_read_auth(&mut interrupt, Duration::from_secs(600)),
        )
        .await
        .expect("the wait must observe an interrupt that predates it");
        assert_eq!(
            outcome,
            LaunchWait::Interrupted(ShutdownSignal::Interrupt),
            "the restore has to run, so the wait must report the interrupt"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_termination_arriving_during_staging_still_triggers_the_restore() {
        use super::{LaunchWait, wait_for_codex_to_read_auth};
        use crate::signals::{RAISE_LOCK, ShutdownListener, ShutdownSignal};
        use std::time::Duration;
        use tokio::signal::unix::{SignalKind, signal};

        let _raise = RAISE_LOCK.lock().await;
        let mut shutdown = ShutdownListener::new().expect("shutdown listener");
        let mut witness = signal(SignalKind::terminate()).expect("witness listener");

        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        witness.recv().await;

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_codex_to_read_auth(&mut shutdown, Duration::from_secs(600)),
        )
        .await
        .expect("the wait must observe a termination that predates it");
        assert_eq!(outcome, LaunchWait::Interrupted(ShutdownSignal::Terminate));
        assert_eq!(ShutdownSignal::Terminate.exit_code(), 143);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn one_tui_listener_survives_sequential_launches_and_catches_shutdown() {
        use super::{LaunchWait, wait_for_codex_to_read_auth};
        use crate::signals::{RAISE_LOCK, ShutdownListener, ShutdownSignal};
        use std::time::Duration;
        use tokio::signal::unix::{SignalKind, signal};

        let _raise = RAISE_LOCK.lock().await;
        let mut shutdown = ShutdownListener::new().expect("TUI shutdown listener");

        for _ in 0..2 {
            assert_eq!(
                wait_for_codex_to_read_auth(&mut shutdown, Duration::from_millis(1)).await,
                LaunchWait::Elapsed
            );
        }

        let mut witness = signal(SignalKind::terminate()).expect("witness listener");
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        witness.recv().await;

        assert_eq!(
            wait_for_codex_to_read_auth(&mut shutdown, Duration::from_secs(600)).await,
            LaunchWait::Interrupted(ShutdownSignal::Terminate),
            "the same TUI-owned listener must handle shutdown after multiple launches"
        );
    }

    /// The ordinary path: nothing interrupts, so the wait just times out and
    /// the restore runs on schedule.
    #[tokio::test]
    async fn an_uninterrupted_wait_reports_the_elapsed_delay() {
        use super::{LaunchWait, wait_for_codex_to_read_auth};
        use crate::signals::{RAISE_LOCK, ShutdownListener};
        use std::time::Duration;

        // Not raising anything, but a sibling test does, and a raise is
        // process-wide: without the lock this listener can catch it.
        let _raise = RAISE_LOCK.lock().await;
        let mut interrupt = ShutdownListener::new().expect("shutdown listener");
        assert_eq!(
            wait_for_codex_to_read_auth(&mut interrupt, Duration::from_millis(10)).await,
            LaunchWait::Elapsed
        );
    }

    struct TestAppHome {
        _lock: MutexGuard<'static, ()>,
        home: tempfile::TempDir,
        previous: Option<OsString>,
    }

    impl TestAppHome {
        fn new() -> Self {
            let lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("PAPER_CLAUDE_SWITCH_HOME");
            unsafe {
                std::env::set_var("PAPER_CLAUDE_SWITCH_HOME", home.path());
            }
            Self {
                _lock: lock,
                home,
                previous,
            }
        }

        fn path(&self) -> &std::path::Path {
            self.home.path()
        }
    }

    impl Drop for TestAppHome {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("PAPER_CLAUDE_SWITCH_HOME", value),
                    None => std::env::remove_var("PAPER_CLAUDE_SWITCH_HOME"),
                }
            }
        }
    }

    #[test]
    fn restore_launch_auth_restores_original_and_removes_backup() {
        let home = TestAppHome::new();
        let codex_auth = home.path().join("codex/auth.json");
        let backup = home.path().join("auth.backup");
        std::fs::create_dir_all(codex_auth.parent().unwrap()).unwrap();
        std::fs::write(&codex_auth, b"staged profile").unwrap();
        std::fs::write(&backup, b"original auth").unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(std::fs::read(&codex_auth).unwrap(), b"original auth");
        assert!(!backup.exists());
        assert!(home.path().join("auth.lock").exists());
    }

    #[test]
    fn restore_launch_auth_removes_staged_auth_without_original() {
        let home = TestAppHome::new();
        let codex_auth = home.path().join("codex/auth.json");
        let backup = home.path().join("auth.backup");
        std::fs::create_dir_all(codex_auth.parent().unwrap()).unwrap();
        std::fs::write(&codex_auth, b"staged profile").unwrap();

        restore_launch_auth(&codex_auth, &backup, false, "work").unwrap();

        assert!(!codex_auth.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn restore_launch_auth_without_original_or_staged_file_is_noop() {
        let home = TestAppHome::new();
        let codex_auth = home.path().join("codex/auth.json");
        let backup = home.path().join("auth.backup");

        restore_launch_auth(&codex_auth, &backup, false, "work").unwrap();

        assert!(!codex_auth.exists());
        assert!(!backup.exists());
    }

    // ── Atomic write contract ───────────────────────────────────────
    //
    // Both the backup and the restore write the live auth.json, which holds a
    // one-time-use refresh_token: a crash mid-write must never leave a
    // truncated file, and the file must never be group/world readable. These
    // are the two observable differences between `atomic_write_private` and
    // `std::fs::copy` (which preserves source permissions and copies bytes
    // in place rather than via a temp file + rename), so we assert on them
    // rather than trying to simulate a crash directly.

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn backup_launch_auth_writes_backup_with_private_permissions() {
        let home = TestAppHome::new();
        let codex_auth = home.path().join("codex/auth.json");
        let backup = home.path().join("auth.backup");
        std::fs::create_dir_all(codex_auth.parent().unwrap()).unwrap();
        // Default `fs::write` permissions (governed by umask) are not 0600,
        // so this only passes if the backup path went through the private
        // atomic writer rather than a permission-preserving copy.
        std::fs::write(&codex_auth, b"live credentials").unwrap();

        backup_launch_auth(&codex_auth, &backup).unwrap();

        assert_eq!(std::fs::read(&backup).unwrap(), b"live credentials");
        assert_eq!(mode(&backup), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn restore_launch_auth_writes_target_with_private_permissions() {
        let home = TestAppHome::new();
        let codex_auth = home.path().join("codex/auth.json");
        let backup = home.path().join("auth.backup");
        std::fs::create_dir_all(codex_auth.parent().unwrap()).unwrap();
        std::fs::write(&codex_auth, b"staged profile").unwrap();
        std::fs::write(&backup, b"original auth").unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(std::fs::read(&codex_auth).unwrap(), b"original auth");
        assert_eq!(mode(&codex_auth), 0o600);
    }

    #[test]
    fn restore_launch_auth_leaves_no_stray_files_when_target_already_existed() {
        let home = TestAppHome::new();
        let codex_dir = home.path().join("codex");
        let codex_auth = codex_dir.join("auth.json");
        let backup = home.path().join("auth.backup");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(&codex_auth, b"staged profile").unwrap();
        std::fs::write(&backup, b"original auth").unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        let entries: Vec<_> = std::fs::read_dir(&codex_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("auth.json")],
            "no leftover temp file should remain next to the restored auth.json"
        );
    }

    // ── Codex-side refresh during the launch window ───────────────
    //
    // Codex CLI refreshes a staged auth.json whose `last_refresh` is old
    // enough, and OpenAI revokes the old refresh_token the moment it is used.
    // The restore must therefore fold a newer live copy back into the profile
    // instead of rolling the backup over it.

    fn jwt(payload: &serde_json::Value) -> String {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap())
        )
    }

    /// `account` seeds both the email and the account id, so two calls with the
    /// same `account` describe the same ChatGPT account.
    fn auth_value(account: &str, refresh_token: &str, last_refresh: &str) -> serde_json::Value {
        let email = format!("{account}@example.com");
        let account_id = format!("acct-{account}");
        let claims = serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "chatgpt_account_id": account_id,
                "chatgpt_user_id": format!("user_{account_id}"),
            }
        });
        serde_json::json!({
            "tokens": {
                "id_token": jwt(&claims),
                "access_token": format!("access-{refresh_token}"),
                "refresh_token": refresh_token,
                "account_id": account_id,
            },
            "last_refresh": last_refresh,
        })
    }

    /// Profile "work" plus a staged live file holding the same credentials,
    /// mirroring the state `stage_profile_auth` leaves behind.
    fn staged_launch(
        home: &TestAppHome,
        profile_value: &serde_json::Value,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let profile_path = crate::profile::profile_auth_path("work").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        crate::auth::write_auth(&profile_path, profile_value).unwrap();

        let codex_auth = home.path().join("codex/auth.json");
        std::fs::create_dir_all(codex_auth.parent().unwrap()).unwrap();
        crate::auth::write_auth(&codex_auth, profile_value).unwrap();

        let backup = home.path().join("auth.backup");
        crate::auth::write_auth(
            &backup,
            &auth_value("other", "other-refresh", "2026-07-01T00:00:00Z"),
        )
        .unwrap();

        (profile_path, codex_auth, backup)
    }

    fn read_json(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn restore_saves_credentials_codex_refreshed_during_launch() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);

        // Codex rotated the token in place while it was staged.
        let refreshed = auth_value("a", "refresh-new", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &refreshed).unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(
            read_json(&profile_path),
            refreshed,
            "the rotated refresh_token must survive the restore"
        );
        assert_eq!(
            read_json(&codex_auth)["tokens"]["refresh_token"],
            "other-refresh",
            "the original live credentials must still be restored"
        );
        assert!(!backup.exists());
    }

    #[test]
    fn audit_same_account_restore_keeps_rotated_live_credentials() {
        let home = TestAppHome::new();
        let old = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &old);
        crate::auth::write_auth(&backup, &old).unwrap();

        let new = auth_value("a", "refresh-new", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &new).unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(read_json(&profile_path), new);
        assert_eq!(
            read_json(&codex_auth),
            new,
            "same-account restore must keep rotated credentials live"
        );
    }

    #[test]
    fn restore_restores_a_distinct_same_account_backup_after_refresh() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-staged", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);

        let original = auth_value("a", "refresh-backup", "2026-07-01T00:00:00Z");
        crate::auth::write_auth(&backup, &original).unwrap();
        let refreshed = auth_value("a", "refresh-new", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &refreshed).unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(read_json(&profile_path), refreshed);
        assert_eq!(read_json(&codex_auth), original);
        assert!(!backup.exists());
    }

    #[test]
    fn restore_rechecks_managed_workspace_policy_before_saving_refreshed_credentials() {
        let home = TestAppHome::new();
        let staged = auth_value("allowed", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);
        let refreshed = auth_value("allowed", "refresh-new", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &refreshed).unwrap();

        let codex_home = home.path().join("codex");
        std::fs::write(
            codex_home.join("config.toml"),
            "forced_login_method = \"chatgpt\"\nforced_chatgpt_workspace_id = \"acct-blocked\"\n",
        )
        .unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        unsafe {
            std::env::set_var("CODEX_HOME", &codex_home);
        }
        let result = restore_launch_auth(&codex_auth, &backup, true, "work");
        unsafe {
            match previous_codex_home {
                Some(value) => std::env::set_var("CODEX_HOME", value),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }

        let err = result.expect_err("policy changes during launch must fail closed");
        assert!(format!("{err:#}").contains("not allowed"));
        assert_eq!(read_json(&profile_path), staged);
        assert_eq!(
            read_json(&codex_auth),
            refreshed,
            "the only rotated credential copy must remain recoverable"
        );
        assert!(backup.exists());
    }

    #[test]
    fn restore_saves_refreshed_credentials_when_there_was_no_original() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);
        std::fs::remove_file(&backup).unwrap();

        let refreshed = auth_value("a", "refresh-new", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &refreshed).unwrap();

        restore_launch_auth(&codex_auth, &backup, false, "work").unwrap();

        assert_eq!(read_json(&profile_path), refreshed);
        assert!(!codex_auth.exists());
    }

    #[test]
    fn restore_leaves_profile_untouched_when_codex_did_not_refresh() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(read_json(&profile_path), staged);
        assert_eq!(
            read_json(&codex_auth)["tokens"]["refresh_token"],
            "other-refresh"
        );
        assert!(!backup.exists());
    }

    #[test]
    fn restore_ignores_live_credentials_older_than_the_profile() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-new", "2026-07-20T10:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);

        // A stale copy of the same account must never be written back.
        crate::auth::write_auth(
            &codex_auth,
            &auth_value("a", "refresh-dead", "2026-07-01T00:00:00Z"),
        )
        .unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(read_json(&profile_path), staged);
    }

    // ── Rollback must never destroy credentials it could not archive ──
    //
    // Once preserving fails, the live auth.json may hold the only refresh_token
    // that still works (OpenAI revokes the previous one the moment Codex uses
    // it). Rolling the backup over it, or deleting it, is unrecoverable; the
    // cost of *not* rolling back is one `paper-claude-switch use <alias>`.

    #[test]
    fn restore_keeps_live_credentials_it_could_not_preserve() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);

        // Newer, but not this profile's account: it cannot be folded into the
        // profile, and it is the only copy of whatever was logged in there.
        let foreign = auth_value("b", "refresh-b", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &foreign).unwrap();

        let err = restore_launch_auth(&codex_auth, &backup, true, "work").unwrap_err();

        assert_eq!(
            read_json(&profile_path),
            staged,
            "another account's credentials must not pollute this profile"
        );
        assert_eq!(
            read_json(&codex_auth),
            foreign,
            "the rollback must not overwrite credentials it failed to archive"
        );
        assert!(
            backup.exists(),
            "the pre-launch auth.json must stay on disk so the user can converge by hand"
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&backup.display().to_string()) && msg.contains("paper-claude-switch import"),
            "the refusal must name the backup and how to recover, got: {msg}"
        );
    }

    #[test]
    fn restore_keeps_live_credentials_it_could_not_preserve_without_an_original() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);
        std::fs::remove_file(&backup).unwrap();

        let foreign = auth_value("b", "refresh-b", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &foreign).unwrap();

        let err = restore_launch_auth(&codex_auth, &backup, false, "work").unwrap_err();

        assert_eq!(
            read_json(&codex_auth),
            foreign,
            "deleting the staged file would destroy the only copy of these credentials"
        );
        assert_eq!(read_json(&profile_path), staged);
        assert!(format!("{err:#}").contains("paper-claude-switch import"));
    }
}
