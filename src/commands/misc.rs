use crate::output::print_json;
use crate::{auth, claude_store, claude_usage, color};
use anyhow::{Context, Result};

// ── open ─────────────────────────────────────────────────

pub(crate) fn open_cmd() -> Result<()> {
    let dir = auth::app_home()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating directory {}", dir.display()))?;
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer.exe")
        .arg(dir.as_os_str())
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&dir).spawn();
    match result {
        Ok(_) => println!("Opened: {}", dir.display()),
        Err(e) => println!(
            "{}",
            color::error(&format!(
                "Could not open file manager: {e}\nPath: {}",
                dir.display()
            ))
        ),
    }
    Ok(())
}

// ── doctor ────────────────────────────────────────────────

/// First executable called `name` on `PATH`.
fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    let suffixes: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| {
        suffixes
            .iter()
            .map(|suffix| dir.join(format!("{name}{suffix}")))
            .find(|candidate| candidate.is_file())
    })
}

pub(crate) fn doctor_cmd(json: bool) -> Result<()> {
    let paths = claude_usage::paths()?;
    let mut problems: Vec<String> = Vec::new();

    let claude_path = find_on_path("claude");
    let claude_version = claude_path.as_ref().and_then(|path| {
        let output = std::process::Command::new(path).arg("--version").output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status.success() && !text.is_empty()).then_some(text)
    });
    if claude_path.is_none() {
        problems.push("`claude` was not found on PATH; install Claude Code".into());
    } else if claude_version.is_none() {
        problems.push("`claude --version` did not report a version".into());
    }

    let live = match claude_store::read_live(&paths) {
        Ok(Some(live)) => Some(live),
        Ok(None) => {
            problems.push("no live Claude login; run `claude`, then `/login`".into());
            None
        }
        Err(error) => {
            problems.push(format!("cannot read the Claude login: {error:#}"));
            None
        }
    };
    if cfg!(target_os = "macos") {
        problems.push("Keychain not supported yet: macOS stores Claude credentials in the Keychain".into());
    }

    let ok = problems.is_empty();
    if json {
        print_json(&serde_json::json!({
            "ok": ok,
            "claude_path": claude_path.as_ref().map(|p| p.display().to_string()),
            "claude_version": claude_version,
            "credentials_path": paths.credentials.display().to_string(),
            "global_config_path": paths.global_config.display().to_string(),
            "live_login": live.is_some(),
            "live_email": live.as_ref().and_then(|l| l.email.clone()),
            "problems": problems,
        }));
    } else {
        let line = |good: bool, text: String| {
            if good {
                println!("{}", color::success(&format!("[ok] {text}")));
            } else {
                println!("{}", color::error(&format!("[!!] {text}")));
            }
        };
        match (&claude_path, &claude_version) {
            (Some(path), Some(version)) => line(true, format!("claude {version} ({})", path.display())),
            (Some(path), None) => line(false, format!("claude at {} did not report a version", path.display())),
            (None, _) => line(false, "claude not found on PATH".into()),
        }
        println!("    credentials: {}", paths.credentials.display());
        println!("    global config: {}", paths.global_config.display());
        match &live {
            Some(live) => line(
                true,
                format!("live login: {}", live.email.as_deref().unwrap_or("unknown email")),
            ),
            None => line(false, "no live Claude login".into()),
        }
        for problem in &problems {
            println!("{}", color::error(&format!("Problem: {problem}")));
        }
    }
    if ok { Ok(()) } else { Err(crate::output::OutputAlreadyReported.into()) }
}

