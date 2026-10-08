use crate::output::print_json;
use anyhow::Result;

// ── self-update ──────────────────────────────────────────

const UPDATE_HINT: &str = "update with `paper-claude-switch self-update` from the npm install, or reinstall (`npm i -g paper-claude-switch@latest` / `cargo install --git https://github.com/Cazorlas/Paper.Claude-switcher`)";

/// Reached only when the binary runs directly: an npm install routes
/// `self-update` to the launcher, which owns the update.
pub(crate) fn self_update_cmd(check: bool, json: bool) -> Result<()> {
    if check {
        if json {
            print_json(&serde_json::json!({
                "current_version": crate::update::current_version(),
                "latest_version": null,
                "update_available": null,
                "hint": UPDATE_HINT,
            }));
        } else {
            println!(
                "paper-claude-switch {} (this binary was run directly; {UPDATE_HINT})",
                crate::update::current_version()
            );
        }
        return Ok(());
    }
    anyhow::bail!("this binary was run directly; {UPDATE_HINT}")
}
