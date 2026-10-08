use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::claude_store::{self, ImportKind, ProfileImporter};
use crate::output::print_json;
use crate::{auth, color};

// ── import-orca ───────────────────────────────────────────

const HINT: &str = "Remove these accounts from Orca (Settings > Accounts) so only one app refreshes each login.";

#[derive(Serialize)]
struct ImportedAccount {
    alias: String,
    email: Option<String>,
    action: &'static str,
    source: String,
}

#[derive(Serialize)]
struct SkippedAccount {
    source: String,
    reason: String,
}

#[derive(Serialize)]
struct ImportReport {
    accounts: Vec<ImportedAccount>,
    skipped: Vec<SkippedAccount>,
    dry_run: bool,
}

fn default_orca_dir() -> Result<PathBuf> {
    let config = dirs::config_dir().context("could not determine the config directory")?;
    Ok(config.join("orca").join("claude-accounts"))
}

fn read_json_file(path: &Path) -> Result<Value> {
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("{} is not valid JSON", path.display()))
}

/// The account Orca keeps in `<dir>/<id>/auth`, or why it cannot be imported.
fn read_orca_account(dir: &Path) -> Result<claude_store::LiveAccount> {
    let auth_dir = dir.join("auth");
    if !auth_dir.is_dir() {
        bail!("no auth folder");
    }
    let credentials = read_json_file(&auth_dir.join(".credentials.json"))?;
    let account = read_json_file(&auth_dir.join("oauth-account.json"))?;
    claude_store::account_from_parts(&credentials, &account)
}

/// Copy the Claude accounts Orca manages into profiles. Orca's files are only read.
pub(crate) fn import_orca_cmd(from: Option<&Path>, dry_run: bool, json: bool) -> Result<()> {
    let orca_dir = match from {
        Some(path) => path.to_path_buf(),
        None => default_orca_dir()?,
    };
    if !orca_dir.is_dir() {
        bail!("Orca accounts folder not found: {}", orca_dir.display());
    }
    let mut ids: Vec<String> = Vec::new();
    for entry in fs::read_dir(&orca_dir).with_context(|| format!("reading {}", orca_dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            ids.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    ids.sort();

    let app_home = auth::app_home()?;
    let _lock = if dry_run { None } else { Some(crate::profile::lock_live_auth()?) };
    let mut importer = ProfileImporter::new(&app_home, dry_run)?;
    let mut report = ImportReport { accounts: Vec::new(), skipped: Vec::new(), dry_run };
    for id in ids {
        let incoming = match read_orca_account(&orca_dir.join(&id)) {
            Ok(account) => account,
            Err(error) => {
                report.skipped.push(SkippedAccount { source: id, reason: format!("{error:#}") });
                continue;
            }
        };
        let email = incoming.email.clone();
        let outcome = importer.import(incoming)?;
        let action = match outcome.kind {
            ImportKind::Created => "created",
            ImportKind::Updated => "updated",
            ImportKind::Kept => "kept",
        };
        tracing::info!(action = "import-orca", alias = %outcome.alias, outcome = action, dry_run, "orca account imported");
        report.accounts.push(ImportedAccount { alias: outcome.alias, email, action, source: id });
    }

    if json {
        print_json(&report);
        return Ok(());
    }
    for account in &report.accounts {
        let email = account.email.as_deref().unwrap_or("unknown");
        println!("{}", color::success(&format!("{} {} ({email})", account.action, account.alias)));
    }
    for skipped in &report.skipped {
        println!("skipped {}: {}", skipped.source, skipped.reason);
    }
    if dry_run {
        println!("Dry run: nothing was written.");
    }
    if !report.accounts.is_empty() {
        println!("{HINT}");
    }
    Ok(())
}
