use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    /// Detect terminal capabilities automatically
    Auto,
    /// Always use colors
    Always,
    /// Never use colors
    Never,
}

#[derive(Parser)]
#[command(
    name = "paper-claude-switch",
    version = concat!(env!("CARGO_PKG_VERSION"), "\n", env!("CARGO_PKG_REPOSITORY")),
    about = "Claude Code account switcher - multi-profile manager with usage dashboard\nhttps://github.com/Cazorlas/Paper.Claude-switcher",
    long_about = None,
    after_help = "Examples:\n  paper-claude-switch list\n  paper-claude-switch use\n  paper-claude-switch rename old-alias new-alias\n  paper-claude-switch self-update --check\n\nRun `paper-claude-switch <command> --help` for command-specific options."
)]
pub struct Cli {
    /// Output as compact JSON
    #[arg(long, global = true)]
    pub json: bool,

    /// Output as pretty-printed JSON
    #[arg(long, global = true)]
    pub json_pretty: bool,

    /// Proxy URL (overrides CS_PROXY / HTTP_PROXY / HTTPS_PROXY / ALL_PROXY env vars)
    ///
    /// Supported formats:
    ///   http://[user:pass@]host:port
    ///   https://[user:pass@]host:port
    ///   socks4://host:port
    ///   socks5://[user:pass@]host:port      (local DNS)
    ///   socks5h://[user:pass@]host:port     (remote DNS)
    #[arg(long, global = true, env = "CS_PROXY")]
    pub proxy: Option<String>,

    /// Color output mode
    #[arg(long, global = true, default_value = "auto", env = "CS_COLOR")]
    pub color: ColorMode,

    /// Enable debug logging (shows HTTP status, retries, and cache status)
    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Switch to a profile; omit alias to auto-select using the unified scoring algorithm
    Use {
        /// Profile alias or its number in `list` (omit to auto-select the best)
        alias: Option<String>,
    },
    /// Watch the active account and switch to a better one before it hits its usage limit
    #[command(
        after_help = "Polls the active account's 5h and 7d windows. When either reaches --threshold, switches to the eligible account with the most headroom (never to one that is itself over the threshold or within --margin points of the current one). A cooldown stops flip-flopping; when every account is exhausted it backs off to a slow cadence.

`auto` runs in this window: leave it open (you can minimize it); stop it with Ctrl+C or by closing the window.

--once does a single check for cron: exit 0 switched, 1 error, 2 nothing to do, 3 blocked (no viable target).

Examples:
  paper-claude-switch auto
  paper-claude-switch auto --threshold 80 --dry-run
  paper-claude-switch --json auto --once"
    )]
    Auto {
        /// Switch when the 5h or 7d window reaches this used percent
        #[arg(long, default_value_t = 90.0)]
        threshold: f64,
        /// Minimum points a target must be below the active account
        #[arg(long, default_value_t = 10.0)]
        margin: f64,
        /// Seconds between checks
        #[arg(long, default_value_t = 60)]
        interval: u64,
        /// Minimum seconds between two switches
        #[arg(long, default_value_t = 300)]
        cooldown: u64,
        /// Check once and exit (for cron/scripts)
        #[arg(long)]
        once: bool,
        /// Report what would happen without switching
        #[arg(long)]
        dry_run: bool,
    },
    /// List all profiles with account info, usage, and availability
    List {
        /// Force refresh, bypass cache
        #[arg(long, short)]
        force: bool,
    },
    /// Rename a profile
    Rename {
        /// Current profile alias
        old: String,
        /// New profile alias
        new: String,
    },
    /// Delete a profile (archived for recovery)
    Delete {
        /// Profile alias
        alias: String,
        /// Skip the confirmation prompt (required with --json or when stdin is not a terminal)
        #[arg(long, short)]
        yes: bool,
    },
    /// List deleted profiles, or restore one (newest archive) by alias
    Restore {
        /// Deleted profile alias (omit to list what can be restored)
        alias: Option<String>,
        /// Restore under a different alias
        #[arg(long = "as", value_name = "NEW_ALIAS")]
        as_alias: Option<String>,
    },
    /// Save the account Claude Code is logged in to (log in with `claude`, then `/login`, first)
    Login {
        /// Profile alias (default: derived from the account email; a saved account keeps its alias)
        alias: Option<String>,
    },
    /// Import the Claude accounts the Orca app manages as profiles
    #[command(
        after_help = "Reads <orca>/<id>/auth/{.credentials.json,oauth-account.json} for each Orca account. A new account becomes a profile; a saved account is updated only when Orca holds the fresher login; anything else is kept as is. Orca's files are never changed.\n\nExamples:\n  paper-claude-switch import-orca --dry-run\n  paper-claude-switch import-orca\n  paper-claude-switch --json import-orca --from /path/to/claude-accounts"
    )]
    ImportOrca {
        /// Orca accounts folder (default: <config dir>/orca/claude-accounts)
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Report what would be imported without writing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Check for a newer version (`--check`) or update this binary
    #[command(
        after_help = "Examples:\n  paper-claude-switch self-update --check\n  paper-claude-switch self-update\n\nOnly the TUI checks automatically at startup. Other commands never check automatically."
    )]
    SelfUpdate {
        /// Check whether a newer version is available without installing it
        #[arg(long)]
        check: bool,
    },
    /// Switch to a profile, then run Claude Code
    #[command(after_help = "Pass Claude Code arguments after --.
Example: paper-claude-switch launch work -- --resume")]
    Launch {
        /// Profile alias (omit to auto-select the best profile)
        alias: Option<String>,
        /// Always None; the `--model` flag was removed (pass it after `--`).
        #[arg(skip)]
        model: Option<String>,
        /// Claude Code argv; prefer `--` before this so flags are not parsed by paper-claude-switch
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Feed Claude Code's status-line usage into the cache (no network); use as the status line command
    #[command(
        after_help = "Claude Code pipes a JSON object with `rate_limits` to the status line command. This reads it for the active account and updates the usage cache, so `list`, the TUI and `auto` need no usage request for it. Pass your existing status line command after -- and its output is printed unchanged.\n\nExample settings.json:\n  \"statusLine\": {\"type\": \"command\", \"command\": \"paper-claude-switch statusline -- <your existing status line command>\"}"
    )]
    Statusline {
        /// Next status line command and its arguments (after --); stdin is passed on to it
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Launch the interactive TUI
    Tui,
    /// Open the paper-claude-switch data directory (~/.paper-claude-switch, or $PAPER_CLAUDE_SWITCH_HOME) in the system file manager
    Open,
    /// Check Claude Code, the credentials location and the current login
    Doctor,
}

/// Split `paper-claude-switch launch …` so Claude Code argv is never parsed as a
/// paper-claude-switch alias or global flag.
///
/// Clap treats a bare `--` as "stop parsing flags" but still fills the next
/// positional, so `launch -- work` would otherwise become alias `work`. A
/// known Claude Code subcommand (`mcp`, `config`, …) or a non-launch flag (`-p`)
/// in the alias slot is treated the same way, so `launch mcp --json` is not
/// `profile 'mcp' not found`.
pub(crate) fn extract_launch_passthrough(argv: &[String]) -> (Vec<String>, Option<Vec<String>>) {
    let Some(launch_at) = first_subcommand(argv).filter(|&i| argv[i] == "launch") else {
        return (argv.to_vec(), None);
    };
    let mut i = launch_at + 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "--" {
            return (argv[..i].to_vec(), Some(argv[i + 1..].to_vec()));
        }
        if let Some(skip) = skip_launch_or_global_flag(argv, i) {
            i += skip;
            continue;
        }
        if arg.starts_with('-') || is_claude_subcommand(arg) {
            return (argv[..i].to_vec(), Some(argv[i..].to_vec()));
        }
        if let Some(rel) = argv[i + 1..].iter().position(|next| next == "--") {
            let dash = i + 1 + rel;
            return (argv[..dash].to_vec(), Some(argv[dash + 1..].to_vec()));
        }
        return (argv.to_vec(), None);
    }
    (argv.to_vec(), None)
}

/// Concatenate clap's trailing launch args with argv taken from after `--`
/// (or from a Claude Code subcommand / foreign flag). `launch work mcp -- --json`
/// must keep `mcp`.
pub(crate) fn merge_launch_args(
    clap_args: Vec<String>,
    passthrough: Option<Vec<String>>,
) -> Vec<String> {
    match passthrough {
        Some(right) => {
            let mut args = clap_args;
            args.extend(right);
            args
        }
        None => clap_args,
    }
}

fn skip_launch_or_global_flag(argv: &[String], i: usize) -> Option<usize> {
    let arg = argv[i].as_str();
    if arg == "-h" || arg == "-V" {
        return Some(1);
    }
    let rest = arg.strip_prefix("--")?;
    if rest.is_empty() {
        return None;
    }
    let (name, has_eq) = match rest.split_once('=') {
        Some((name, _)) => (name, true),
        None => (rest, false),
    };
    match name {
        "json" | "json-pretty" | "debug" | "help" | "version" => Some(1),
        "proxy" | "color" => {
            if has_eq || i + 1 >= argv.len() || argv[i + 1] == "--" || argv[i + 1].starts_with('-')
            {
                Some(1)
            } else {
                Some(2)
            }
        }
        _ => None,
    }
}

/// Claude Code's own subcommands: in the alias slot of `launch` they start
/// the passthrough instead of naming a profile.
pub(crate) fn is_claude_subcommand(name: &str) -> bool {
    matches!(
        name,
        "mcp"
            | "config"
            | "plugin"
            | "doctor"
            | "update"
            | "install"
            | "setup-token"
            | "migrate-installer"
            | "help"
    )
}

fn first_subcommand(argv: &[String]) -> Option<usize> {
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "--" {
            return None;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            if rest.is_empty() {
                return None;
            }
            i += if rest.contains('=') || !global_value_flag(rest) {
                1
            } else {
                2
            };
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(i);
    }
    None
}

fn global_value_flag(name: &str) -> bool {
    matches!(name, "proxy" | "color")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    fn parse_launch(raw_args: &[&str]) -> (bool, Option<String>, Option<String>, Vec<String>) {
        let raw = argv(raw_args);
        let (left, right) = extract_launch_passthrough(&raw);
        let cli = Cli::try_parse_from(&left).unwrap_or_else(|e| panic!("{e}"));
        match cli.command {
            Commands::Launch {
                alias, model, args, ..
            } => (cli.json, alias, model, merge_launch_args(args, right)),
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn launch_passthrough_after_double_dash_keeps_claude_exec_json_and_color() {
        let (json, alias, model, args) = parse_launch(&[
            "paper-claude-switch",
            "launch",
            "work",
            "--",
            "mcp",
            "--json",
            "--color",
            "never",
            "do the thing",
        ]);
        assert!(!json, "Claude Code --json after -- must not turn on cs --json");
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(model, None);
        assert_eq!(args, ["mcp", "--json", "--color", "never", "do the thing"]);
    }

    #[test]
    fn launch_without_double_dash_must_not_steal_claude_exec_json() {
        let (json, alias, model, args) = parse_launch(&[
            "paper-claude-switch",
            "launch",
            "work",
            "mcp",
            "--json",
            "do the thing",
        ]);
        assert!(
            !json,
            "cs --json is global and currently steals Claude Code exec --json; this test names the contract"
        );
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(model, None);
        assert_eq!(args, ["mcp", "--json", "do the thing"]);
    }

    #[test]
    fn launch_cs_json_before_double_dash_still_applies_to_claude_switch() {
        let (json, alias, _, args) = parse_launch(&[
            "paper-claude-switch",
            "--json",
            "launch",
            "work",
            "--",
            "mcp",
            "--json",
        ]);
        assert!(json);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(args, ["mcp", "--json"]);
    }

    #[test]
    fn launch_model_after_double_dash_is_claude_model_not_cs_model() {
        let (_, alias, model, args) = parse_launch(&[
            "paper-claude-switch",
            "launch",
            "openrouter",
            "--",
            "--model",
            "opus",
        ]);
        assert_eq!(alias.as_deref(), Some("openrouter"));
        assert_eq!(model, None);
        assert_eq!(args, ["--model", "opus"]);
    }

    #[test]
    fn launch_double_dash_then_alias_shaped_prompt_is_passthrough_not_alias() {
        let raw = argv(&["paper-claude-switch", "launch", "--", "work"]);
        let (left, right) = extract_launch_passthrough(&raw);
        let cli = Cli::try_parse_from(&left).unwrap_or_else(|e| panic!("{e}"));
        match cli.command {
            Commands::Launch { alias, args, .. } => {
                assert_eq!(alias, None);
                assert!(args.is_empty(), "clap argv stops before --");
            }
            other => panic!("expected launch, got {other:?}"),
        }
        assert_eq!(right, Some(argv(&["work"])));
    }

    #[test]
    fn extract_launch_passthrough_keeps_exec_json_after_separator() {
        let raw = argv(&[
            "paper-claude-switch",
            "--json",
            "launch",
            "work",
            "--",
            "mcp",
            "--json",
            "do the thing",
        ]);
        let (left, right) = extract_launch_passthrough(&raw);
        let cli = Cli::try_parse_from(&left).unwrap_or_else(|e| panic!("{e}"));
        assert!(cli.json);
        match cli.command {
            Commands::Launch {
                alias, model, args, ..
            } => {
                assert_eq!(alias.as_deref(), Some("work"));
                assert_eq!(model, None);
                assert!(args.is_empty());
            }
            other => panic!("expected launch, got {other:?}"),
        }
        assert_eq!(right, Some(argv(&["mcp", "--json", "do the thing"])));
    }

    #[test]
    fn extract_launch_passthrough_preserves_claude_double_dash() {
        let raw = argv(&[
            "paper-claude-switch",
            "launch",
            "work",
            "--",
            "mcp",
            "--",
            "--looks-like-flag",
        ]);
        let (_, right) = extract_launch_passthrough(&raw);
        assert_eq!(right, Some(argv(&["mcp", "--", "--looks-like-flag"])));
    }

    #[test]
    fn launch_cs_json_flag_on_the_subcommand_is_for_claude_switch() {
        let (json, alias, _, args) = parse_launch(&["paper-claude-switch", "launch", "work", "--json"]);
        assert!(json);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(args, [] as [&str; 0]);
    }

    #[test]
    fn launch_without_double_dash_must_not_steal_claude_exec_color() {
        let cli = Cli::try_parse_from([
            "paper-claude-switch",
            "launch",
            "work",
            "mcp",
            "--color",
            "never",
            "do",
        ])
        .unwrap_or_else(|e| panic!("{e}"));
        match cli.command {
            Commands::Launch { alias, args, .. } => {
                assert_eq!(alias.as_deref(), Some("work"));
                assert_eq!(
                    cli.color,
                    ColorMode::Auto,
                    "Claude Code exec --color must not change cs --color"
                );
                assert_eq!(args, ["mcp", "--color", "never", "do"]);
            }
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn launch_passthrough_keeps_sandbox_and_cd_flags() {
        let (_, alias, _, args) = parse_launch(&[
            "paper-claude-switch",
            "launch",
            "work",
            "--",
            "-s",
            "workspace-write",
            "-C",
            "/tmp/proj",
            "-a",
            "never",
            "ship it",
        ]);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(
            args,
            [
                "-s",
                "workspace-write",
                "-C",
                "/tmp/proj",
                "-a",
                "never",
                "ship it"
            ]
        );
    }

    #[test]
    fn launch_merges_args_on_both_sides_of_double_dash() {
        let (json, alias, _, args) = parse_launch(&[
            "paper-claude-switch",
            "launch",
            "work",
            "mcp",
            "--",
            "--json",
            "hi",
        ]);
        assert!(!json);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(args, ["mcp", "--json", "hi"]);
    }

    #[test]
    fn launch_exec_without_double_dash_is_claude_not_an_alias() {
        let (json, alias, _, args) =
            parse_launch(&["paper-claude-switch", "launch", "mcp", "--json", "do the thing"]);
        assert!(
            !json,
            "Claude Code exec --json must not turn on cs --json when exec is the first token"
        );
        assert_eq!(alias, None);
        assert_eq!(args, ["mcp", "--json", "do the thing"]);
    }

    #[test]
    fn launch_sandbox_flag_without_double_dash_is_claude_not_a_parse_error() {
        let (_, alias, _, args) = parse_launch(&[
            "paper-claude-switch",
            "launch",
            "work",
            "--",
            "-s",
            "workspace-write",
        ]);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(args, ["-s", "workspace-write"]);

        let (_, alias, _, args) = parse_launch(&[
            "paper-claude-switch",
            "launch",
            "-s",
            "workspace-write",
            "-a",
            "never",
        ]);
        assert_eq!(alias, None);
        assert_eq!(args, ["-s", "workspace-write", "-a", "never"]);
    }

    #[test]
    fn launch_claude_flag_without_alias_is_passthrough() {
        let (_, alias, _, args) =
            parse_launch(&["paper-claude-switch", "launch", "--continue"]);
        assert_eq!(alias, None);
        assert_eq!(args, ["--continue"]);
    }

    #[test]
    fn launch_claude_subcommand_without_alias_is_passthrough() {
        let (_, alias, _, args) =
            parse_launch(&["paper-claude-switch", "launch", "mcp", "list"]);
        assert_eq!(alias, None);
        assert_eq!(args, ["mcp", "list"]);
    }

    #[test]
    fn merge_launch_args_keeps_left_tokens_then_right() {
        assert_eq!(
            merge_launch_args(argv(&["mcp"]), Some(argv(&["--json", "hi"]))),
            argv(&["mcp", "--json", "hi"])
        );
        assert_eq!(merge_launch_args(argv(&["mcp"]), None), argv(&["mcp"]));
    }
}
