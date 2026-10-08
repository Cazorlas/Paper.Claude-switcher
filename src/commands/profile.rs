use super::render::{confirm_default_no, print_usage_line};
use crate::claude_store::{self, LiveAccount, LockOptions, SwitchOutcome};
use crate::claude_usage;
use crate::output::{
    self, ProgressReporter, account_to_json, print_json, usage_to_json, user_println,
};
use crate::{auth, cache, color, config, profile, usage};
use anyhow::{Context, Result};

/// The live Claude login and the saved profiles, with the active profile
/// resolved by account uuid.
struct Accounts {
    live: Option<LiveAccount>,
    profiles: Vec<claude_usage::Profile>,
    active: Option<String>,
}

impl Accounts {
    fn load() -> Result<Self> {
        let paths = claude_usage::paths()?;
        let live = claude_store::read_live(&paths)?;
        let mut profiles = claude_usage::profiles()?;
        let active = claude_usage::active_alias(&profiles, live.as_ref())?;
        // The live plan is fresher than the one stored with the active profile.
        if let (Some(live), Some(active)) = (&live, &active) {
            if let Some(plan) = live.oauth["subscriptionType"].as_str() {
                for p in profiles.iter_mut().filter(|p| &p.alias == active) {
                    p.info.plan_type = Some(plan.to_owned());
                }
            }
        }
        Ok(Self { live, profiles, active })
    }

    fn live_oauth(&self) -> Option<&LiveAccount> {
        self.live.as_ref()
    }

    fn find(&self, alias: &str) -> Option<&claude_usage::Profile> {
        self.profiles.iter().find(|p| p.alias == alias)
    }

    fn switch(&self, alias: &str) -> Result<SwitchOutcome> {
        if let Some(live) = &self.live {
            if self.active.is_none() {
                anyhow::bail!(
                    "the live Claude account ({}) is not saved; run `paper-claude-switch login` first",
                    live.email.as_deref().unwrap_or("unknown email")
                );
            }
        }
        let outcome = profile::switch_profile(alias)?;
        cache::set_last_used(alias)?;
        tracing::info!(
            action = "switch",
            alias,
            outcome = "completed",
            "account switched"
        );
        Ok(outcome)
    }
}

// ── use ──────────────────────────────────────────────────

pub(crate) async fn use_cmd(alias: Option<&str>, json: bool) -> Result<()> {
    let Some(requested) = alias else {
        return best_cmd(json).await;
    };
    let accounts = Accounts::load()?;
    // `use 2` picks the 2nd profile in `list` order (1-based), unless a profile is
    // literally named "2".
    let by_index = resolve_profile_index(requested, &accounts.profiles);
    let alias = by_index.as_deref().unwrap_or(requested);
    let outcome = accounts.switch(alias)?;
    let action = match outcome {
        SwitchOutcome::Switched { .. } => "switched",
        SwitchOutcome::AlreadyActive => "already_active",
    };
    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias: alias.to_string(),
            action: action.into(),
        });
    }
    // Human message too: stdout in text mode, stderr beside the JSON report.
    if action == "switched" {
        user_println(&color::success(&format!("Switched to: {alias}")));
    } else {
        user_println(&color::dim(&format!("Already using: {alias}")));
    }
    Ok(())
}

/// Map a 1-based position in `list` order to a profile alias. `None` when the
/// text is not a number, is out of range, or names an existing profile.
fn resolve_profile_index(text: &str, profiles: &[claude_usage::Profile]) -> Option<String> {
    let n: usize = text.parse().ok()?;
    if profiles.iter().any(|p| p.alias == text) {
        return None;
    }
    profiles.get(n.checked_sub(1)?).map(|p| p.alias.clone())
}

// ── list (all profiles + usage, concurrent) ──────────────

pub(crate) async fn list_cmd(force: bool, json: bool) -> Result<()> {
    let accounts = Accounts::load()?;
    if accounts.profiles.is_empty() {
        if json {
            print_json(&output::JsonUsageResult { profiles: vec![] });
        } else {
            println!(
                "{}",
                color::dim("(no saved profiles; run `paper-claude-switch login`)")
            );
        }
        return Ok(());
    }

    // Only accounts without a fresh cache entry need a network round trip.
    let stale = accounts
        .profiles
        .iter()
        .filter(|p| force || cache::get(&p.alias).is_none())
        .count();
    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Refreshing usage", stale))
    };
    let results = claude_usage::fetch_all(
        &accounts.profiles,
        accounts.active.as_deref(),
        accounts.live_oauth(),
        force,
    )
    .await;
    if let Some(progress) = progress.as_mut() {
        progress.advance(stale);
        progress.finish();
    }

    let mut json_items = vec![];
    for (position, (p, usage_result)) in accounts.profiles.iter().zip(results).enumerate() {
        let is_current = accounts.active.as_deref() == Some(p.alias.as_str());
        if json {
            let ju = match &usage_result {
                Ok(u) => usage_to_json(Ok(u)),
                Err(e) => usage_to_json(Err(&e.detail)),
            };
            json_items.push(output::JsonProfileWithUsage {
                alias: p.alias.clone(),
                is_current,
                account: account_to_json(&p.info, None),
                usage: ju,
            });
        } else {
            let mark = if is_current {
                color::active("*")
            } else {
                " ".to_string()
            };
            let alias_str = if is_current {
                color::bold(&p.alias)
            } else {
                p.alias.clone()
            };
            print!("{mark} {} {alias_str}", color::dim(&format!("{}.", position + 1)));
            if let Some(email) = &p.info.email {
                print!("  {}", color::dim(email));
            }
            if let Some(plan) = p.info.plan_type.as_deref() {
                print!("  {}", color::plan(plan, Some(plan)));
            }
            println!();
            match usage_result {
                Ok(u) => print_usage_line(&u),
                Err(e) => println!("  {} {}", color::error("!!"), color::error(&e.summary)),
            }
            println!(); // blank line between accounts
        }
    }

    if json {
        print_json(&output::JsonUsageResult {
            profiles: json_items,
        });
    }
    Ok(())
}

// ── rename ───────────────────────────────────────────────

pub(crate) fn rename_cmd(old: &str, new: &str, json: bool) -> Result<()> {
    profile::rename_profile(old, new)?;
    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias: new.to_string(),
            action: "renamed".into(),
        });
    }
    Ok(())
}

pub(crate) fn restore_cmd(alias: Option<&str>, as_alias: Option<&str>, json: bool) -> Result<()> {
    let Some(alias) = alias else {
        let deleted = profile::list_deleted()?;
        if json {
            let names: Vec<_> = deleted.iter().map(|(a, _)| a.as_str()).collect();
            print_json(&serde_json::json!({ "deleted": names }));
        } else if deleted.is_empty() {
            println!("{}", color::dim("(no deleted profiles)"));
        } else {
            for (alias, archive) in deleted {
                println!("{alias}  {}", color::dim(&archive));
            }
            println!(
                "{}",
                color::dim("restore one with `paper-claude-switch restore <alias>`")
            );
        }
        return Ok(());
    };
    let restored = profile::cmd_restore(alias, as_alias)?;
    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias: restored,
            action: "restored".into(),
        });
    } else {
        println!("{}", color::success(&format!("Restored profile: {restored}")));
    }
    Ok(())
}

pub(crate) fn delete_cmd(alias: &str, yes: bool, json: bool) -> Result<()> {
    use std::io::IsTerminal;

    profile::validate_alias(alias)?;
    if !profile::profile_auth_path(alias)?.exists() {
        anyhow::bail!("profile '{alias}' not found");
    }
    // The live account decides, whatever the `current` marker says.
    if claude_usage::current_active().as_deref() == Some(alias) {
        anyhow::bail!("cannot delete the active profile '{alias}'");
    }

    if !yes {
        if json || !std::io::stdin().is_terminal() {
            anyhow::bail!("confirmation required; rerun with --yes to delete profile '{alias}'");
        }
        if !confirm_default_no(&format!(
            "Delete profile '{alias}'? It will remain recoverable. [y/N] "
        )) {
            user_println("Deletion cancelled.");
            return Ok(());
        }
    }
    profile::cmd_delete(alias)?;
    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias: alias.to_string(),
            action: "deleted".into(),
        });
    }
    Ok(())
}

// ── best (internal, called by `use` with no alias) ────────

/// Score and order candidates: eligible first, then by score, least recently
/// used, and alias.
pub(crate) fn rank_candidates(
    items: Vec<(String, usage::UsageInfo, claude_usage::AccountInfo, i64)>,
    now: i64,
    safety_7d: f64,
    team_priority: bool,
) -> Vec<(usage::Candidate, usage::UsageInfo, f64)> {
    let mut scored: Vec<(usage::Candidate, usage::UsageInfo, f64)> =
        usage::score_candidates(items, now, safety_7d, team_priority)
            .into_iter()
            .map(|s| (s.candidate, s.usage, s.score))
            .collect();

    scored.sort_by(|a, b| {
        let eligible_a = usage::is_candidate_eligible(&a.0, safety_7d);
        let eligible_b = usage::is_candidate_eligible(&b.0, safety_7d);
        eligible_b
            .cmp(&eligible_a)
            .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.0.last_used.cmp(&b.0.last_used))
            .then(a.0.alias.cmp(&b.0.alias))
    });

    scored
}

pub(crate) fn score_profile_candidates(
    fetched: Vec<(String, usage::UsageInfo)>,
    now: i64,
    safety_7d: f64,
    team_priority: bool,
) -> Vec<(usage::Candidate, usage::UsageInfo, f64)> {
    let items = fetched
        .into_iter()
        .map(|(alias, u)| {
            let info = claude_usage::read_profile(&alias)
                .map(|p| p.info)
                .unwrap_or_default();
            let last_used = cache::get_last_used(&alias);
            (alias, u, info, last_used)
        })
        .collect();
    rank_candidates(items, now, safety_7d, team_priority)
}

/// Make `alias` the live Claude account (what `use <alias>` does).
pub(crate) fn switch_alias(alias: &str) -> Result<SwitchOutcome> {
    Accounts::load()?.switch(alias)
}

/// The best profile to use now, by usage; nothing is switched here.
pub(crate) async fn select_best_profile(json: bool) -> Result<SelectOutcome> {
    rank_accounts(&Accounts::load()?, json).await
}

pub(crate) struct SelectOutcome {
    pub(crate) alias: String,
    pub(crate) usage: usage::UsageInfo,
    pub(crate) score: f64,
}

async fn rank_accounts(accounts: &Accounts, json: bool) -> Result<SelectOutcome> {
    if accounts.profiles.is_empty() {
        anyhow::bail!("no saved profiles; run `paper-claude-switch login` first");
    }

    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new(
            "Testing accounts",
            accounts.profiles.len(),
        ))
    };
    let results = claude_usage::fetch_all(
        &accounts.profiles,
        accounts.active.as_deref(),
        accounts.live_oauth(),
        false,
    )
    .await;
    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    let mut items = Vec::new();
    for (p, result) in accounts.profiles.iter().zip(results) {
        match result {
            Ok(u) => items.push((
                p.alias.clone(),
                u,
                p.info.clone(),
                cache::get_last_used(&p.alias),
            )),
            Err(e) => tracing::warn!("[{}] usage fetch failed during auto-select: {}", p.alias, e),
        }
    }
    if items.is_empty() {
        anyhow::bail!("all usage queries failed");
    }

    let cfg = config::get();
    let scored = rank_candidates(
        items,
        auth::now_unix_secs(),
        cfg.use_cfg.safety_margin_7d,
        cfg.use_cfg.team_priority,
    );
    let (top, best_usage, best_score) = scored
        .into_iter()
        .next()
        .context("failed to select best profile")?;
    Ok(SelectOutcome { alias: top.alias, usage: best_usage, score: best_score })
}

async fn best_cmd(json: bool) -> Result<()> {
    let accounts = Accounts::load()?;
    let SelectOutcome { alias: best_alias, usage: best_usage, score: best_score } =
        rank_accounts(&accounts, json).await?;

    accounts.switch(&best_alias)?;
    let info = accounts
        .find(&best_alias)
        .map(|p| p.info.clone())
        .unwrap_or_default();

    if json {
        print_json(&output::JsonBest {
            switched_to: best_alias.clone(),
            account: account_to_json(&info, best_usage.plan_type.as_deref()),
            usage: usage_to_json(Ok(&best_usage)),
            score: best_score,
            mode: "unified".to_string(),
            hint: None,
        });
    } else {
        println!("{}", color::success(&format!("Switched to: {best_alias}")));
        print_usage_line(&best_usage);
    }
    Ok(())
}
