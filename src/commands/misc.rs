use crate::{auth, color};
use anyhow::{Context, Result};

pub(crate) fn format_resync_confirm_prompt(
    alias: &str,
    live_last_refresh: Option<&str>,
    profile_last_refresh: Option<&str>,
) -> String {
    let live_ts = live_last_refresh.unwrap_or("unknown");
    let profile_ts = profile_last_refresh.unwrap_or("unknown");
    format!(
        "Update profile '{alias}' with live credentials? (live last_refresh={live_ts} -> profile last_refresh={profile_ts}) [Y/n] "
    )
}





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

// ── warmup ────────────────────────────────────────────────



#[cfg(test)]
mod tests {
    use super::format_resync_confirm_prompt;

    #[test]
    fn resync_prompt_shows_direction_and_missing_timestamps() {
        let prompt = format_resync_confirm_prompt("acme", Some("2026-07-20T00:00:00Z"), None);

        assert_eq!(
            prompt,
            "Update profile 'acme' with live credentials? (live last_refresh=2026-07-20T00:00:00Z -> profile last_refresh=unknown) [Y/n] "
        );
    }
}
