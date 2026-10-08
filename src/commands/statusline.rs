use std::io::{Read, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::usage::WindowUsage;
use crate::{cache, claude_store, claude_usage};

/// `statusline [-- <next command...>]`: feed Claude Code's `rate_limits` into
/// the usage cache without any network call. Whatever goes wrong inside, the
/// status line still gets the next command's output (or nothing) and exit 0.
/// With `tee`, stdin goes back out unchanged instead of the summary line, so
/// the command can feed another status line through a shell pipe.
pub(crate) fn statusline_cmd(tee: bool, next: Vec<String>) -> Result<()> {
    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        input.clear();
    }
    let line = match record_rate_limits(&input) {
        Ok(line) => line,
        Err(error) => {
            tracing::debug!("statusline: {error:#}");
            None
        }
    };
    if tee {
        let mut out = std::io::stdout();
        let _ = out.write_all(&input);
        let _ = out.flush();
        return Ok(());
    }
    if next.is_empty() {
        if let Some(line) = line {
            let _ = writeln!(std::io::stdout(), "{line}");
        }
        return Ok(());
    }
    let code = run_next(&next, input);
    std::io::stdout().flush().ok();
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Store the windows in `input` for the live account's profile and return the
/// one-line summary; `None` when no saved profile matches the live login.
fn record_rate_limits(input: &[u8]) -> Result<Option<String>> {
    let body: Value = serde_json::from_slice(input).context("stdin is not JSON")?;
    let live = claude_store::read_live(&claude_usage::paths()?)?;
    let Some(live) = live else { return Ok(None) };
    let alias = crate::profile::list_profiles()?
        .iter()
        .filter_map(|alias| claude_usage::read_profile(alias).ok())
        .find(|profile| profile.info.account_id.as_deref() == Some(live.account_uuid.as_str()))
        .map(|profile| profile.alias);
    let Some(alias) = alias else { return Ok(None) };

    let limits = body.get("rate_limits");
    let read = |key: &str, minutes: i64| -> Option<WindowUsage> {
        let window = limits?.get(key)?;
        Some(WindowUsage {
            used_percent: Some(window.get("used_percentage")?.as_f64()?),
            resets_at: window
                .get("resets_at")
                .and_then(|at| at.as_i64().or_else(|| at.as_f64().map(|at| at as i64))),
            window_minutes: Some(minutes),
        })
    };
    let five_hour = read("five_hour", 300);
    let seven_day = read("seven_day", 10_080);
    if five_hour.is_some() || seven_day.is_some() {
        cache::put_live_windows(&alias, five_hour.clone(), seven_day.clone())?;
    }

    let percent = |window: &Option<WindowUsage>, label: &str| {
        window
            .as_ref()
            .and_then(|window| window.used_percent)
            .map(|used| format!("{label} {:.0}%", used.round()))
    };
    let parts: Vec<String> = [percent(&five_hour, "5h"), percent(&seven_day, "7d")]
        .into_iter()
        .flatten()
        .collect();
    let mut line = alias;
    if !parts.is_empty() {
        line.push(' ');
        line.push_str(&parts.join(" \u{b7} "));
    }
    Ok(Some(line))
}

/// Run the next status line command with the same stdin; its stdout and
/// stderr go straight through. Returns the exit code to use.
fn run_next(next: &[String], input: Vec<u8>) -> i32 {
    let Ok(mut child) = Command::new(&next[0])
        .args(&next[1..])
        .stdin(Stdio::piped())
        .spawn()
    else {
        return 0;
    };
    let writer = child.stdin.take().map(|mut stdin| {
        std::thread::spawn(move || {
            // The command may exit without reading all of it.
            let _ = stdin.write_all(&input);
        })
    });
    let status = child.wait();
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 0,
    }
}
