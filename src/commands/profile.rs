use super::render::{confirm_default_no, print_usage_line};
use crate::output::{
    self, ProgressReporter, account_to_json, print_json, usage_to_json, user_println,
};
use crate::{auth, cache, color, config, jwt, profile, usage, workspace};
use anyhow::{Context, Result};



/// Surface profiles whose rotated credentials could not be written.
///
/// The auth server has already invalidated their previous refresh token, so
/// staying quiet hands the user an account that stops working later with no
/// clue why. Printed to stderr so `--json` stdout stays machine-readable.
fn report_token_persist_failures(failures: &[usage::TokenPersistFailure]) {
    for failure in failures {
        eprintln!(
            "{}",
            color::error(&format!("Warning: {}", failure.error.detail))
        );
    }
}

// ── use ──────────────────────────────────────────────────

pub(crate) async fn use_cmd(alias: Option<&str>, json: bool) -> Result<()> {
    use std::io::IsTerminal;

    auth::ensure_file_credentials_store()?;

    // `use 2` picks the 2nd profile in `list` order (1-based), unless a profile is
    // literally named "2".
    let by_index = alias.and_then(resolve_profile_index);
    let alias = by_index.as_deref().or(alias);
    match alias {
        Some(a) => {
            profile::cmd_use(a, !json && std::io::stdin().is_terminal())?;
            cache::set_last_used(a)?;
            tracing::info!(
                action = "switch",
                alias = a,
                outcome = "completed",
                "account switched"
            );
            if json {
                print_json(&output::JsonOk {
                    ok: true,
                    alias: a.to_string(),
                    action: "switched".into(),
                });
            }
        }
        None => best_cmd(json).await?,
    }
    Ok(())
}

/// Map a 1-based position in `list` order to a profile alias. `None` when the
/// text is not a number, is out of range, or names an existing profile.
fn resolve_profile_index(text: &str) -> Option<String> {
    let n: usize = text.parse().ok()?;
    let profiles = profile::list_profiles().ok()?;
    if profiles.iter().any(|p| p == text) {
        return None;
    }
    profiles.get(n.checked_sub(1)?).cloned()
}

// ── list (all profiles + usage, concurrent) ──────────────

pub(crate) async fn list_cmd(force: bool, json: bool, auth_already_handled: bool) -> Result<()> {
    if !auth_already_handled {
        profile::auto_track_current();
    }

    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        if json {
            print_json(&output::JsonUsageResult { profiles: vec![] });
        } else {
            println!(
                "{}",
                color::dim(
                    "(no saved profiles; run `paper-claude-switch login`)"
                )
            );
        }
        return Ok(());
    }

    let current = profile::read_current();

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    struct ListRow {
        name: String,
        is_current: bool,
        info: jwt::AccountInfo,
        usage_result: Option<std::result::Result<usage::UsageInfo, usage::UsageError>>,
    }

    let mut rows: Vec<ListRow> = profiles
        .into_iter()
        .filter_map(|name| {
            let path = match profile::profile_auth_path(&name) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("[{name}] failed to resolve profile path: {e}");
                    return None;
                }
            };
            let info = auth::read_account_info(&path);
            let usage_result = if force {
                None
            } else {
                cache::get(&name).map(Ok)
            };
            Some(ListRow {
                is_current: name == current,
                name,
                info,
                usage_result,
            })
        })
        .collect();

    let refresh_count = rows.iter().filter(|row| row.usage_result.is_none()).count();
    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Refreshing usage", refresh_count))
    };

    let mut tasks = tokio::task::JoinSet::new();
    for (idx, row) in rows.iter().enumerate() {
        let needs_usage = row.usage_result.is_none();
        let needs_workspace = force
            || row
                .info
                .account_id
                .as_deref()
                .is_some_and(|id| !cache::workspace_name_is_known(id));
        if !needs_usage && !needs_workspace {
            continue;
        }

        let alias = row.name.clone();
        let current = current.clone();
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return (
                    idx,
                    needs_usage.then(|| {
                        Err(usage::UsageError {
                            summary: "limiter closed".into(),
                            detail: "usage limiter closed".into(),
                        })
                    }),
                );
            };
            let path = match profile::profile_auth_path(&alias) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        idx,
                        needs_usage.then(|| {
                            Err(usage::UsageError {
                                summary: format!("path error: {e}"),
                                detail: format!("failed to resolve profile path: {e}"),
                            })
                        }),
                    );
                }
            };
            let usage_result = if needs_usage {
                Some(if force {
                    usage::fetch_usage_retried_force(&alias, &path, &current).await
                } else {
                    usage::fetch_usage_retried(&alias, &path, &current).await
                })
            } else {
                None
            };
            // Read auth after usage: that path may have refreshed and persisted the token.
            if let Ok(auth) = auth::read_auth(&path)
                && let Err(err) = workspace::refresh_for_auth_if_needed(&auth, force).await
            {
                tracing::debug!("[{alias}] workspace metadata unavailable: {err}");
            }
            (idx, usage_result)
        });
    }

    let mut completed = 0usize;
    while let Some(task) = tasks.join_next().await {
        let (idx, usage_result) = task.map_err(|e| anyhow::anyhow!("usage worker failed: {e}"))?;
        if let Some(usage_result) = usage_result {
            rows[idx].usage_result = Some(usage_result);
            completed += 1;
        }
        cache::apply_workspace_name(&mut rows[idx].info);
        if let Some(progress) = progress.as_mut() {
            progress.advance(completed);
        }
    }

    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    let mut json_items = vec![];

    for (position, row) in rows.into_iter().enumerate() {
        let usage_result = row.usage_result.unwrap_or_else(|| {
            Err(usage::UsageError {
                summary: "unknown".into(),
                detail: "usage result missing".into(),
            })
        });
        if json {
            let ju = match &usage_result {
                Ok(u) => usage_to_json(Ok(u)),
                Err(e) => usage_to_json(Err(&e.detail)),
            };
            json_items.push(output::JsonProfileWithUsage {
                alias: row.name,
                is_current: row.is_current,
                account: account_to_json(
                    &row.info,
                    usage_result
                        .as_ref()
                        .ok()
                        .and_then(|u| u.plan_type.as_deref()),
                ),
                usage: ju,
            });
        } else {
            let mark = if row.is_current {
                color::active("*")
            } else {
                " ".to_string()
            };
            let alias_str = if row.is_current {
                color::bold(&row.name)
            } else {
                row.name.clone()
            };
            print!("{mark} {} {alias_str}", color::dim(&format!("{}.", position + 1)));
            if let Some(email) = &row.info.email {
                print!("  {}", color::dim(email));
            }
            // API plan_type is authoritative over JWT claims (handles plan downgrades)
            let effective_plan = if let Ok(u) = &usage_result {
                u.plan_type.as_deref().or(row.info.plan_type.as_deref())
            } else {
                row.info.plan_type.as_deref()
            };
            if effective_plan.is_some() {
                let label = if let Ok(u) = &usage_result
                    && u.plan_type.is_some()
                {
                    row.info.plan_label_with(u.plan_type.as_deref())
                } else {
                    row.info.plan_label()
                };
                print!("  {}", color::plan(&label, effective_plan));
            }
            let (exp_text, exp_level) =
                jwt::subscription_label(row.info.subscription_until, auth::now_unix_secs());
            if exp_level != jwt::ExpiryLevel::Unknown {
                let text = format!("until {exp_text}");
                match exp_level {
                    jwt::ExpiryLevel::Soon => print!("  {}", color::warn(&text)),
                    jwt::ExpiryLevel::Past => print!("  {}", color::dim(&text)),
                    _ => print!("  {}", color::dim(&text)),
                }
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

    // Opportunistically refresh tokens about to expire (background, bounded)
    report_token_persist_failures(&usage::refresh_expiring_tokens().await);

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
    if profile::read_current() == alias {
        anyhow::bail!("cannot delete the active profile '{alias}'");
    }
    if !profile::profile_auth_path(alias)?.exists() {
        anyhow::bail!("profile '{alias}' not found");
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

pub(crate) fn score_profile_candidates(
    fetched: Vec<(String, usage::UsageInfo)>,
    now: i64,
    safety_7d: f64,
    team_priority: bool,
) -> Vec<(usage::Candidate, usage::UsageInfo, f64)> {
    let items = fetched
        .into_iter()
        .map(|(alias, u)| {
            let info = profile::profile_auth_path(&alias)
                .map(|p| auth::read_account_info(&p))
                .unwrap_or_default();
            let last_used = cache::get_last_used(&alias);
            (alias, u, info, last_used)
        })
        .collect();

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

pub(crate) async fn select_best_profile(
    json: bool,
) -> Result<SelectOutcome> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        anyhow::bail!(
            "no saved profiles; run `paper-claude-switch login` first"
        );
    }

    let current = profile::read_current();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    let mut tasks = tokio::task::JoinSet::new();
    let mut fetched: Vec<(String, usage::UsageInfo)> = Vec::with_capacity(profiles.len());

    for alias in profiles {
        if let Some(cached) = cache::get_async(&alias).await {
            fetched.push((alias, cached));
            continue;
        }

        let current = current.clone();
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return None;
            };
            let path = match profile::profile_auth_path(&alias) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("[{alias}] failed to resolve profile path: {e}");
                    return None;
                }
            };
            match usage::fetch_usage_retried(&alias, &path, &current).await {
                Ok(u) => Some((alias, u)),
                Err(e) => {
                    tracing::warn!("[{alias}] usage fetch failed during auto-select: {e}");
                    None
                }
            }
        });
    }

    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Testing accounts", tasks.len()))
    };

    let mut completed = 0usize;
    while let Some(task) = tasks.join_next().await {
        completed += 1;
        if let Some(progress) = progress.as_mut() {
            progress.advance(completed);
        }
        if let Some((alias, usage)) =
            task.map_err(|e| anyhow::anyhow!("usage worker failed: {e}"))?
        {
            fetched.push((alias, usage));
        }
    }

    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    if fetched.is_empty() {
        anyhow::bail!("all usage queries failed");
    }

    let safety_7d = config::get().use_cfg.safety_margin_7d;
    let team_priority = config::get().use_cfg.team_priority;
    let now = auth::now_unix_secs();
    let scored = score_profile_candidates(fetched, now, safety_7d, team_priority);
    let (top_candidate, top_usage, top_score) = scored
        .first()
        .map(|(c, u, s)| (c.clone(), u.clone(), *s))
        .context("failed to select best profile")?;

    Ok(SelectOutcome { alias: top_candidate.alias, usage: top_usage, score: top_score })
}

pub(crate) struct SelectOutcome {
    pub(crate) alias: String,
    pub(crate) usage: usage::UsageInfo,
    pub(crate) score: f64,
}

async fn best_cmd(json: bool) -> Result<()> {
    let outcome = select_best_profile(json).await?;
    let SelectOutcome {
        alias: best_alias,
        usage: best_usage,
        score: best_score,
    } = outcome;

    profile::switch_profile(&best_alias)?;
    cache::set_last_used(&best_alias)?;
    tracing::info!(
        action = "switch",
        alias = %best_alias,
        outcome = "completed",
        selection = "automatic",
        "account switched"
    );

    let path = profile::profile_auth_path(&best_alias)?;
    let info = auth::read_account_info(&path);

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

    // Opportunistically refresh tokens about to expire (background, bounded)
    report_token_persist_failures(&usage::refresh_expiring_tokens().await);

    Ok(())
}

