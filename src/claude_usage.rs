use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::DateTime;
use serde_json::Value;

use crate::claude_api::{ClaudeUsage, Endpoints, UsageError as ApiError};
use crate::claude_store::{self, ClaudePaths, LiveAccount};
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

/// Identity of a saved Claude account, read from the profile's account.json
/// and credentials.json.
#[derive(Debug, Default, Clone)]
pub struct AccountInfo {
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub account_id: Option<String>,
    pub workspace_name: Option<String>,
    /// End of the current paid period (unix seconds), when known.
    pub subscription_until: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Free,
    Go,
    Plus,
    ProLite,
    Pro,
    Team,
    Business,
    Enterprise,
    Edu,
    Unknown,
}

impl PlanKind {
    pub fn from_wire(plan_type: Option<&str>) -> Self {
        match plan_type {
            Some("free") => Self::Free,
            Some("go") => Self::Go,
            Some("plus") => Self::Plus,
            Some("prolite") => Self::ProLite,
            Some("pro") => Self::Pro,
            Some("team") => Self::Team,
            Some("self_serve_business_usage_based" | "business") => Self::Business,
            Some("enterprise_cbp_usage_based" | "enterprise") => Self::Enterprise,
            Some("education" | "edu") => Self::Edu,
            _ => Self::Unknown,
        }
    }

    fn display_name(self, raw: Option<&str>) -> String {
        match self {
            Self::Free => "Free".to_string(),
            Self::Go => "Go".to_string(),
            Self::Plus => "Plus".to_string(),
            Self::ProLite => "Pro 5×".to_string(),
            Self::Pro => "Pro 20×".to_string(),
            Self::Team => "Team".to_string(),
            Self::Business => "Business".to_string(),
            Self::Enterprise => "Enterprise".to_string(),
            Self::Edu => "Edu".to_string(),
            Self::Unknown => raw.unwrap_or("?").to_string(),
        }
    }
}

impl AccountInfo {
    pub fn plan_label(&self) -> String {
        self.plan_label_with(self.plan_type.as_deref())
    }

    /// Same as `plan_label` but with an overridden plan type (e.g. from API response).
    pub fn plan_label_with(&self, plan_type: Option<&str>) -> String {
        let base = PlanKind::from_wire(plan_type).display_name(plan_type);
        if let Some(name) = &self.workspace_name
            && !name.is_empty()
        {
            return format!("{base} - {name}");
        }
        base
    }

    pub fn is_free(&self) -> bool {
        matches!(self.plan_type.as_deref(), Some("free") | None)
    }

    pub fn is_team(&self) -> bool {
        matches!(self.plan_type.as_deref(), Some("team")) || self.workspace_name.is_some()
    }
}

/// How close the end of the paid period is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryLevel {
    Unknown,
    Ok,
    /// Ends within a week.
    Soon,
    /// The recorded end is in the past.
    Past,
}

/// Short text for a table cell, e.g. `10-11 (5d)`, plus its urgency.
pub fn subscription_label(until: Option<i64>, now: i64) -> (String, ExpiryLevel) {
    let Some(until) = until else {
        return ("--".into(), ExpiryLevel::Unknown);
    };
    if until <= now {
        return ("lapsed?".into(), ExpiryLevel::Past);
    }
    let days = (until - now + 86_399) / 86_400;
    let date = chrono::DateTime::from_timestamp(until, 0)
        .map(|utc| utc.with_timezone(&chrono::Local).format("%m-%d").to_string())
        .unwrap_or_else(|| "--".into());
    let level = if days <= 7 { ExpiryLevel::Soon } else { ExpiryLevel::Ok };
    (format!("{date} ({days}d)"), level)
}

#[derive(Clone)]
pub struct Profile {
    pub alias: String,
    pub dir: PathBuf,
    pub info: AccountInfo,
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

pub fn read_profile(alias: &str) -> Result<Profile> {
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
        info: AccountInfo {
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

/// The alias of the saved profile whose account is the live Claude login, read
/// quietly (no warnings for broken profile folders) for the TUI and `auto`.
pub fn current_active() -> Option<String> {
    let live = claude_store::read_live(&paths().ok()?).ok().flatten();
    let profiles: Vec<Profile> = crate::profile::list_profiles()
        .ok()?
        .iter()
        .filter_map(|alias| read_profile(alias).ok())
        .collect();
    active_alias(&profiles, live.as_ref()).ok().flatten()
}

/// Usage for one saved profile by alias: the active account (by live
/// accountUuid) is read with the live token, every other one with its saved
/// token. `force` skips the usage cache.
pub async fn fetch_alias(alias: &str, force: bool) -> Result<UsageInfo, UsageError> {
    let failed = |summary: &str, error: anyhow::Error| UsageError {
        summary: summary.to_owned(),
        detail: format!("{error:#}"),
    };
    let profile = read_profile(alias).map_err(|e| failed("profile unreadable", e))?;
    let live = paths()
        .and_then(|paths| claude_store::read_live(&paths))
        .map_err(|e| failed("Claude login unreadable", e))?;
    let live_oauth = live
        .as_ref()
        .filter(|live| profile.info.account_id.as_deref() == Some(live.account_uuid.as_str()))
        .map(|live| &live.oauth);
    fetch(&profile, live_oauth, force).await
}

/// True when the saved token of an inactive profile is about to be refreshed.
fn needs_refresh(profile: &Profile) -> bool {
    let Ok(bytes) = std::fs::read(profile.dir.join("credentials.json")) else {
        return false;
    };
    let Ok(credentials) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    credentials["claudeAiOauth"]["expiresAt"]
        .as_i64()
        .is_some_and(|expires| expires <= chrono::Utc::now().timestamp_millis() + 300_000)
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
    // `auto` and the TUI may run in other processes: hold the app lock while a
    // rotated token is refreshed and written, so a switch in another process
    // never activates a token this one has just replaced. The refresh re-reads
    // the profile file under the lock.
    let _refresh_lock = if live_oauth.is_none() && needs_refresh(profile) {
        let locked = tokio::task::spawn_blocking(crate::profile::lock_live_auth)
            .await
            .map_err(|e| UsageError {
                summary: "lock failed".into(),
                detail: e.to_string(),
            })?;
        Some(locked.map_err(|e| UsageError {
            summary: "another switch is running".into(),
            detail: format!("{e:#}"),
        })?)
    } else {
        None
    };
    // Another process may have switched to this profile between the caller's
    // "inactive" decision and the lock: re-read the live login now and, when it
    // is this account, read it with the live token instead of refreshing.
    let rechecked = match (&_refresh_lock, live_oauth) {
        (Some(_), None) => paths()
            .and_then(|paths| claude_store::read_live(&paths))
            .map_err(|e| UsageError {
                summary: "Claude login unreadable".into(),
                detail: format!("{e:#}"),
            })?
            .filter(|live| profile.info.account_id.as_deref() == Some(live.account_uuid.as_str()))
            .map(|live| live.oauth),
        _ => None,
    };
    let live_oauth = live_oauth.or(rechecked.as_ref());
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
