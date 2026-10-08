use crate::claude_store::{self, LockOptions, SaveAction};
use crate::output::{self, print_json};
use crate::{auth, claude_usage, color};
use anyhow::Result;

// ── login ─────────────────────────────────────────────────

/// Save the account Claude Code is logged in to as a profile.
pub(crate) fn login_cmd(alias: Option<&str>, json: bool) -> Result<()> {
    let paths = claude_usage::paths()?;
    if claude_store::read_live(&paths)?.is_none() {
        anyhow::bail!(
            "no Claude Code login found; log in to Claude Code first (run `claude`, then `/login`), then run `paper-claude-switch login` again"
        );
    }
    let action = claude_store::save_current(&paths, &auth::app_home()?, alias, &LockOptions::default())?;
    let (alias, outcome, text) = match action {
        SaveAction::Created(a) => {
            let text = format!("[ok] Saved the current Claude login as new profile: {a}");
            (a, "created", text)
        }
        SaveAction::Updated(a) => {
            let text = format!("[ok] Updated existing profile for this account: {a}");
            (a, "updated", text)
        }
    };
    tracing::info!(action = "login", alias = %alias, outcome, "profile login completed");
    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias,
            action: outcome.into(),
        });
    } else {
        println!("{}", color::success(&text));
    }
    Ok(())
}
