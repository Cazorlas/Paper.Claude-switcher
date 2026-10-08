use crate::cli::{Cli, Commands, extract_launch_passthrough, merge_launch_args};
use crate::output::{MessageMode, print_error, should_report_error, user_println};
use crate::{auth, claude_store, claude_usage, color, commands, config, logging, output, tui};
use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

// These events retain command failures in file/TUI history. The CLI already
// renders the same error as a human message or JSON, including in debug mode.
const REPORTED_ERROR_TARGET: &str = "claude_switch::reported_error";

struct LogFilters {
    stderr: EnvFilter,
    file: EnvFilter,
    tui: EnvFilter,
}

fn log_filters(debug: bool, rust_log: Option<&str>) -> LogFilters {
    let all_sinks = if debug {
        Some("claude_switch=debug")
    } else {
        rust_log
    };
    if let Some(filter) = all_sinks {
        return LogFilters {
            stderr: EnvFilter::new(filter),
            file: EnvFilter::new(filter),
            tui: EnvFilter::new(filter),
        };
    }

    LogFilters {
        stderr: EnvFilter::new("claude_switch=error"),
        file: EnvFilter::new("claude_switch=info"),
        tui: EnvFilter::new("claude_switch=info"),
    }
}

pub async fn run_cli() {
    let raw: Vec<String> = std::env::args().collect();
    let (clap_argv, launch_passthrough) = extract_launch_passthrough(&raw);
    let cli = Cli::parse_from(&clap_argv);
    let is_tui = matches!(&cli.command, Commands::Tui);
    let use_json = cli.json || cli.json_pretty;
    let message_mode = if is_tui {
        MessageMode::Silent
    } else if use_json {
        MessageMode::Stderr
    } else {
        MessageMode::Stdout
    };

    color::init(cli.color);
    output::set_json_pretty(cli.json_pretty);
    output::set_message_mode(message_mode);
    if let Err(e) = config::init() {
        if use_json {
            print_error(&e.to_string());
        } else {
            eprintln!("{}", color::error(&format!("Error: {e}")));
        }
        std::process::exit(1);
    }

    // Priority: --debug flag > RUST_LOG env > defaults.
    let rust_log = std::env::var("RUST_LOG").ok();
    let filters = log_filters(cli.debug, rust_log.as_deref());
    // File logging failure must not prevent normal account switching.
    let file_writer = match logging::file_log_writer() {
        Ok(writer) => Some(writer),
        Err(error) => {
            eprintln!(
                "{}",
                color::warn(&format!("Warning: file logging is unavailable: {error}"))
            );
            None
        }
    };
    if is_tui {
        use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
        let tui_writer = logging::tui_log_writer();
        let tui_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(tui_writer)
            .with_filter(filters.tui);
        if let Some(file_writer) = file_writer {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer)
                .with_filter(filters.file);
            tracing_subscriber::registry()
                .with(tui_layer)
                .with(file_layer)
                .init();
        } else {
            tracing_subscriber::registry().with(tui_layer).init();
        }
    } else if let Some(file_writer) = file_writer {
        use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .with_filter(filters.stderr)
            .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                metadata.target() != REPORTED_ERROR_TARGET
            }));
        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(file_writer)
            .with_filter(filters.file);
        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(file_layer)
            .init();
    } else {
        use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .with_filter(filters.stderr)
            .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                metadata.target() != REPORTED_ERROR_TARGET
            }));
        tracing_subscriber::registry().with(stderr_layer).init();
    }
    for warning in config::startup_warnings() {
        eprintln!("{}", color::warn(&format!("Warning: {warning}")));
    }
    config::set_cli_proxy(cli.proxy.clone());

    let result = dispatch(cli.command, use_json, launch_passthrough).await;

    if let Err(e) = result {
        if should_report_error(&e) {
            tracing::error!(target: REPORTED_ERROR_TARGET, error = %format!("{e:#}"), "command failed");
            if use_json {
                print_error(&format!("{e:#}"));
            } else {
                eprintln!("{}", color::error(&format!("Error: {e:#}")));
            }
        }
        std::process::exit(1);
    }
}

fn command_name(cmd: &Commands) -> &'static str {
    match cmd {
        Commands::Use { .. } => "use",
        Commands::Auto { .. } => "auto",
        Commands::List { .. } => "list",
        Commands::Rename { .. } => "rename",
        Commands::Delete { .. } => "delete",
        Commands::Restore { .. } => "restore",
        Commands::Login { .. } => "login",
        Commands::SelfUpdate { .. } => "self-update",
        Commands::Launch { .. } => "launch",
        Commands::Tui => "tui",
        Commands::Open => "open",
        Commands::Doctor => "doctor",
    }
}

async fn dispatch(
    cmd: Commands,
    json: bool,
    launch_passthrough: Option<Vec<String>>,
) -> Result<()> {
    let command = command_name(&cmd);
    let started = std::time::Instant::now();
    tracing::debug!(command, "command started");
    // First run in JSON mode (nobody to prompt): the live Claude login becomes
    // the first profile. Without --json it is offered or announced below.
    if json
        && matches!(
            &cmd,
            Commands::List { .. } | Commands::Use { .. } | Commands::Auto { .. } | Commands::Tui
        )
    {
        save_first_live_account();
    }
    if !json
        && !matches!(
            &cmd,
            Commands::Login { .. }
                | Commands::SelfUpdate { .. }
                | Commands::Open
                | Commands::Launch { .. }
                | Commands::Doctor
        )
    {
        offer_to_save_live_account();
    }

    match cmd {
        Commands::Use {
            alias,
        } => commands::use_cmd(alias.as_deref(), json).await?,
        Commands::Auto {
            threshold,
            margin,
            interval,
            cooldown,
            once,
            dry_run,
        } => {
            let opts = commands::AutoOptions {
                threshold,
                margin,
                interval: std::time::Duration::from_secs(interval.max(5)),
                cooldown: std::time::Duration::from_secs(cooldown),
                once,
                dry_run,
                json,
            };
            commands::auto_cmd(opts).await?
        }
        Commands::List { force } => commands::list_cmd(force, json).await?,
        Commands::Rename { old, new } => commands::rename_cmd(&old, &new, json)?,
        Commands::Restore { alias, as_alias } => {
            commands::restore_cmd(alias.as_deref(), as_alias.as_deref(), json)?
        }
        Commands::Delete { alias, yes } => commands::delete_cmd(&alias, yes, json)?,
        Commands::Login { alias } => commands::login_cmd(alias.as_deref(), json)?,
        Commands::SelfUpdate {
            check,
            version,
            dev,
            stable,
        } => commands::self_update_cmd(check, version.as_deref(), dev, stable, json).await?,
        Commands::Launch { alias, args, .. } => {
            let args = merge_launch_args(args, launch_passthrough);
            commands::launch_cmd(alias.as_deref(), args, json).await?
        }
        Commands::Tui => tui::run_tui().await?,
        Commands::Open => commands::open_cmd()?,
        Commands::Doctor => commands::doctor_cmd(json)?,
    }

    tracing::info!(
        command,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "command completed"
    );

    Ok(())
}

// ── startup auth change detection ────────────────────────

/// No profile exists yet and Claude Code has a live login: save it without
/// asking. The notice goes to stderr so `--json` stdout stays pure JSON.
fn save_first_live_account() {
    let Ok(paths) = claude_usage::paths() else { return };
    if !matches!(claude_store::read_live(&paths), Ok(Some(_))) {
        return;
    }
    if !matches!(crate::profile::list_profiles(), Ok(profiles) if profiles.is_empty()) {
        return;
    }
    let saved = auth::app_home().and_then(|home| {
        claude_store::save_current(&paths, &home, None, &claude_store::LockOptions::default())
    });
    match saved {
        Ok(claude_store::SaveAction::Created(alias) | claude_store::SaveAction::Updated(alias)) => {
            eprintln!("{}", color::success(&format!("Saved profile: {alias}")));
        }
        Err(e) => eprintln!("{}", color::error(&format!("Failed to save: {e:#}"))),
    }
}

/// Claude Code is logged in to an account that has no saved profile: say so,
/// and offer to save it when a person is at the keyboard.
fn offer_to_save_live_account() {
    use std::io::{self, IsTerminal};

    let Some(live) = claude_usage::paths()
        .ok()
        .and_then(|paths| claude_store::read_live(&paths).ok().flatten())
    else {
        return;
    };
    let saved = claude_usage::profiles().is_ok_and(|profiles| {
        profiles
            .iter()
            .any(|p| p.info.account_id.as_deref() == Some(live.account_uuid.as_str()))
    });
    if saved {
        return;
    }
    let label = live.email.as_deref().unwrap_or("unknown");
    // Non-interactive stdin — don't prompt, don't silently mutate state
    if !io::stdin().is_terminal() {
        user_println(&format!(
            "Detected an unsaved Claude account ({label}) (run `paper-claude-switch login` to save it)."
        ));
        return;
    }
    user_println(&format!(
        "Detected an unsaved Claude account ({label}) — not in any saved profile."
    ));
    if commands::confirm("Save as a new profile? [Y/n] ") {
        let saved = claude_usage::paths().and_then(|paths| {
            claude_store::save_current(
                &paths,
                &auth::app_home()?,
                None,
                &claude_store::LockOptions::default(),
            )
        });
        match saved {
            Ok(claude_store::SaveAction::Created(alias) | claude_store::SaveAction::Updated(alias)) => {
                user_println(&format!("Profile saved: {alias}"));
            }
            Err(e) => eprintln!("{}", color::error(&format!("Failed to save: {e:#}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_filters_keep_cli_quiet_but_preserve_operation_history() {
        let filters = log_filters(false, None);

        assert_eq!(filters.stderr.to_string(), "claude_switch=error");
        assert_eq!(filters.file.to_string(), "claude_switch=info");
        assert_eq!(filters.tui.to_string(), "claude_switch=info");
    }

    #[test]
    fn log_filter_precedence_is_debug_then_rust_log_then_defaults() {
        let debug = log_filters(true, Some("claude_switch=trace"));
        assert_eq!(debug.stderr.to_string(), "claude_switch=debug");
        assert_eq!(debug.file.to_string(), "claude_switch=debug");
        assert_eq!(debug.tui.to_string(), "claude_switch=debug");

        let rust_log = log_filters(false, Some("claude_switch=warn"));
        assert_eq!(rust_log.stderr.to_string(), "claude_switch=warn");
        assert_eq!(rust_log.file.to_string(), "claude_switch=warn");
        assert_eq!(rust_log.tui.to_string(), "claude_switch=warn");

        let defaults = log_filters(false, None);
        assert_eq!(defaults.stderr.to_string(), "claude_switch=error");
        assert_eq!(defaults.file.to_string(), "claude_switch=info");
    }
}
