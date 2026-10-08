use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::DateTime;
use serde_json::Value;

use crate::claude_api::{ClaudeUsage, Endpoints, UsageError as ApiError};
use crate::claude_store::{ClaudePaths, LiveAccount};
use crate::usage::{AdditionalRateLimit, UsageError, UsageInfo, WindowUsage};

pub fn paths() -> Result<ClaudePaths> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    Ok(ClaudePaths::resolve(config_dir.as_deref(), &home))
}

pub fn endpoints() -> Endpoints {
    let defaults = Endpoints::default();
    Endpoints {
        api_base: std::env::var("CS_CLAUDE_API_BASE").unwrap_or(defaults.api_base),
        token_url: std::env::var("CS_CLAUDE_TOKEN_URL").unwrap_or(defaults.token_url),
    }
}

fn window(pct: f64, resets_at: Option<&str>, minutes: i64) -> WindowUsage {
    WindowUsage {
        used_percent: Some(pct),
        resets_at: resets_at
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp()),
        window_minutes: Some(minutes),
    }
}

pub fn usage_info(usage: ClaudeUsage) -> UsageInfo {
    UsageInfo {
        fetched_at: Some(crate::auth::now_unix_secs()),
        primary: usage.five_hour.map(|w| window(w.pct, w.resets_at.as_deref(), 300)),
        secondary: usage.seven_day.map(|w| window(w.pct, w.resets_at.as_deref(), 10080)),
        additional_limits: usage
            .models
            .into_iter()
            .map(|w| AdditionalRateLimit {
                limit_name: Some(w.name),
                secondary: Some(window(w.pct, w.resets_at.as_deref(), 10080)),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn usage_error(error: ApiError) -> UsageError {
    let summary = match &error {
        ApiError::RateLimited { .. } => "rate limited".to_owned(),
        ApiError::Unauthorized => "sign-in expired; log in again".to_owned(),
        ApiError::TokenExpired => "token expired; use Claude Code once to renew it".to_owned(),
        ApiError::Http(status) => format!("HTTP {status}"),
        ApiError::Network(_) => "network error".to_owned(),
        ApiError::BadResponse(_) => "unexpected response".to_owned(),
    };
    UsageError { summary, detail: format!("Claude usage: {error:?}") }
}

#[derive(Clone)]
pub struct Profile {
    pub alias: String,
    pub dir: PathBuf,
    pub info: crate::jwt::AccountInfo,
}

/// Saved profiles in `list` order. A profile that cannot be read is skipped
/// with a warning so one broken folder does not hide the others.
pub fn profiles() -> Result<Vec<Profile>> {
    let mut found = Vec::new();
    for alias in crate::profile::list_profiles()? {
        match read_profile(&alias) {
            Ok(profile) => found.push(profile),
            Err(error) => eprintln!(
                "{}",
                crate::color::warn(&format!("Warning: skipping profile '{alias}': {error:#}"))
            ),
        }
    }
    Ok(found)
}

fn read_profile(alias: &str) -> Result<Profile> {
    let dir = crate::auth::profiles_dir()?.join(alias);
    let read = |name: &str| -> Result<Value> {
        let path = dir.join(name);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
    };
    let account = read("account.json")?;
    let credentials = read("credentials.json")?;
    Ok(Profile {
        alias: alias.to_owned(),
        dir,
        info: crate::jwt::AccountInfo {
            email: account["emailAddress"].as_str().map(str::to_owned),
            account_id: Some(
                account["accountUuid"]
                    .as_str()
                    .context("account.json has no accountUuid")?
                    .to_owned(),
            ),
            plan_type: credentials["claudeAiOauth"]["subscriptionType"]
                .as_str()
                .map(str::to_owned),
            ..Default::default()
        },
    })
}

/// The profile whose account is the live Claude login. Repairs the `current`
/// marker when it names another profile.
pub fn active_alias(profiles: &[Profile], live: Option<&LiveAccount>) -> Result<Option<String>> {
    let marker = crate::profile::read_current();
    let matches = |p: &&Profile| {
        live.is_some_and(|live| p.info.account_id.as_deref() == Some(live.account_uuid.as_str()))
    };
    let active = profiles
        .iter()
        .filter(matches)
        .find(|p| p.alias == marker)
        .or_else(|| profiles.iter().find(matches))
        .map(|p| p.alias.clone());
    if let Some(alias) = &active {
        if alias != &marker {
            crate::auth::atomic_write_private(&crate::auth::current_file()?, alias.as_bytes())?;
        }
    }
    Ok(active)
}

/// Usage for one profile. `live_oauth` is Some only for the active profile,
/// which is read with the live token and never refreshed from here.
pub async fn fetch(
    profile: &Profile,
    live_oauth: Option<&Value>,
    force: bool,
) -> Result<UsageInfo, UsageError> {
    if !force {
        if let Some(cached) = crate::cache::get(&profile.alias) {
            return Ok(cached);
        }
    }
    let client = crate::auth::build_http_client().map_err(|e| UsageError {
        summary: "HTTP client error".into(),
        detail: e.to_string(),
    })?;
    let raw = crate::claude_api::usage_for_profile(
        &client,
        &endpoints(),
        &profile.dir,
        live_oauth.is_some(),
        live_oauth,
    )
    .await
    .map_err(usage_error)?;
    let usage = usage_info(raw);
    crate::cache::put(&profile.alias, &usage);
    Ok(usage)
}

/// Usage for every profile, in the same order, fetched concurrently.
pub async fn fetch_all(
    profiles: &[Profile],
    active: Option<&str>,
    live: Option<&LiveAccount>,
    force: bool,
) -> Vec<Result<UsageInfo, UsageError>> {
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        crate::config::get().network.max_concurrent,
    ));
    let mut tasks = tokio::task::JoinSet::new();
    for (idx, profile) in profiles.iter().enumerate() {
        let profile = profile.clone();
        let live_oauth = live
            .filter(|_| active == Some(profile.alias.as_str()))
            .map(|live| live.oauth.clone());
        let semaphore = semaphore.clone();
        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            (idx, fetch(&profile, live_oauth.as_ref(), force).await)
        });
    }
    let mut results: Vec<Result<UsageInfo, UsageError>> = profiles
        .iter()
        .map(|_| {
            Err(UsageError {
                summary: "unknown".into(),
                detail: "usage result missing".into(),
            })
        })
        .collect();
    while let Some(task) = tasks.join_next().await {
        match task {
            Ok((idx, result)) => results[idx] = result,
            Err(error) => tracing::warn!("usage worker failed: {error}"),
        }
    }
    results
}
