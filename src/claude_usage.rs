use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::DateTime;
use serde_json::Value;

use crate::claude_api::{ClaudeUsage, Endpoints, SessionReset, UsageError as ApiError};
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
        session_reset: usage.session_reset,
        ..Default::default()
    }
}

/// Table cell for the session-limit reset offer: `ready` (with `N/wk` when
/// more than one reset a week), `next MM-DD HH:MM` in local time when it is
/// used up for now, else `--`. The flag is true when a reset can be used now.
pub fn session_reset_label(reset: Option<&SessionReset>) -> (String, bool) {
    let Some(reset) = reset.filter(|reset| reset.eligible) else {
        return ("--".into(), false);
    };
    if reset.available {
        let text = if reset.resets_per_week > 1 {
            format!("ready {}/wk", reset.resets_per_week)
        } else {
            "ready".into()
        };
        return (text, true);
    }
    let next = reset
        .next_available_at
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&chrono::Local).format("next %m-%d %H:%M").to_string());
    (next.unwrap_or_else(|| "--".into()), false)
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

/// First renewal instant strictly after `now_unix` of a monthly plan created at
/// `created_rfc3339`, in whole seconds.
pub fn next_renewal(created_rfc3339: &str, now_unix: i64) -> Option<i64> {
    use chrono::{Datelike, NaiveDate, Timelike};

    let created = DateTime::parse_from_rfc3339(created_rfc3339).ok()?.with_timezone(&chrono::Utc);
    let time = created.time().with_nanosecond(0)?;
    let base = i64::from(created.year()) * 12 + i64::from(created.month0());
    // Start close to `now`, one month early, so the loop is a few steps long.
    let now_month = chrono::DateTime::from_timestamp(now_unix, 0)
        .map(|now| i64::from(now.year()) * 12 + i64::from(now.month0()))?;
    let first = (now_month - base - 1).max(1);
    for months in first..first + 3 {
        let index = base + months;
        let year = i32::try_from(index.div_euclid(12)).ok()?;
        let month = u32::try_from(index.rem_euclid(12)).ok()? + 1;
        let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
        let last_day = NaiveDate::from_ymd_opt(next_year, next_month, 1)?.pred_opt()?.day();
        let date = NaiveDate::from_ymd_opt(year, month, created.day().min(last_day))?;
        let instant = date.and_time(time).and_utc().timestamp();
        if instant > now_unix {
            return Some(instant);
        }
    }
    None
}

/// Short plan name from the organization's rate-limit tier and type.
pub fn plan_label(
    rate_limit_tier: Option<&str>,
    organization_type: Option<&str>,
    subscription_type: Option<&str>,
) -> String {
    let tier = rate_limit_tier.unwrap_or("");
    if tier.ends_with("max_5x") {
        "max5x".to_owned()
    } else if tier.ends_with("max_20x") {
        "max20x".to_owned()
    } else if organization_type == Some("claude_pro") || subscription_type == Some("pro") {
        "pro".to_owned()
    } else {
        subscription_type.filter(|value| !value.is_empty()).unwrap_or("--").to_owned()
    }
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
    /// The subscription is canceled or its payment failed.
    Bad,
}

/// Table cell for "Plan until": the renewal date, or the end date of a
/// canceled plan, or `past due`. Pro plans may be yearly, so their date is
/// only approximate and gets a `~`.
pub fn plan_until_label(
    until: Option<i64>,
    now: i64,
    subscription_status: Option<&str>,
    plan: Option<&str>,
) -> (String, ExpiryLevel) {
    let (text, level) = subscription_label(until, now);
    let approx = plan == Some("pro") && until.is_some_and(|until| until > now);
    let text = if approx { format!("~{text}") } else { text };
    match subscription_status {
        Some("past_due") => ("past due".into(), ExpiryLevel::Bad),
        Some("canceled") if until.is_some_and(|until| until > now) => {
            let date = text.split(" (").next().unwrap_or(&text);
            (format!("ends {date}"), ExpiryLevel::Bad)
        }
        Some("canceled") => ("canceled".into(), ExpiryLevel::Bad),
        _ => (text, level),
    }
}

/// Age of a reading that is older than the cache would have kept, e.g.
/// `4m ago`; `None` for a fresh one.
pub fn stale_age_label(fetched_at: Option<i64>, now: i64) -> Option<String> {
    let age = now - fetched_at?;
    let ttl = i64::try_from(crate::config::get().cache.ttl).unwrap_or(i64::MAX);
    if age <= ttl.saturating_add(5) {
        return None;
    }
    Some(match age {
        0..=3599 => format!("{}m ago", (age / 60).max(1)),
        3600..=86_399 => format!("{}h ago", age / 3600),
        _ => format!("{}d ago", age / 86_400),
    })
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
    let label = plan_label(
        account["organizationRateLimitTier"].as_str(),
        account["organizationType"].as_str(),
        credentials["claudeAiOauth"]["subscriptionType"].as_str(),
    );
    // Stripe renews a monthly plan on its creation day; other billing has no
    // date Claude keeps locally.
    let subscription_until = (account["billingType"].as_str() == Some("stripe_subscription"))
        .then(|| account["subscriptionCreatedAt"].as_str())
        .flatten()
        .and_then(|created| next_renewal(created, crate::auth::now_unix_secs()));
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
            plan_type: (label != "--").then_some(label),
            subscription_until,
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
    // A 429 pauses this alias: no request until Retry-After has passed.
    if let Some(left_ms) = crate::cache::pause_left_ms(&profile.alias) {
        return paused_answer(&profile.alias, left_ms);
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
    let endpoints = endpoints();
    let (raw, token) = match crate::claude_api::usage_and_token_for_profile(
        &client,
        &endpoints,
        &profile.dir,
        live_oauth.is_some(),
        live_oauth,
    )
    .await
    {
        Ok(fetched) => fetched,
        Err(ApiError::RateLimited { retry_after }) => {
            let wait = retry_after.map_or(DEFAULT_RETRY_AFTER_SECS, |after| after.as_secs());
            let wait_ms = i64::try_from(wait.min(MAX_RETRY_AFTER_SECS) * 1000).unwrap_or(0);
            crate::cache::pause_for_ms(&profile.alias, wait_ms);
            return paused_answer(&profile.alias, wait_ms);
        }
        Err(error) => return Err(usage_error(error)),
    };
    let mut usage = usage_info(raw);
    crate::cache::put(&profile.alias, &usage);
    // Same token, at most every 6 h; a failure never fails the row, and the
    // token is never refreshed for it.
    if crate::cache::profile_status_due(&profile.alias) {
        let status = crate::claude_api::fetch_profile(&client, &endpoints, &token)
            .await
            .ok()
            .and_then(|status| status.subscription_status);
        crate::cache::put_profile_status(&profile.alias, status);
    }
    usage.subscription_status = crate::cache::profile_status(&profile.alias);
    Ok(usage)
}

/// Retry-After when the 429 carries none, and the longest pause honored.
const DEFAULT_RETRY_AFTER_SECS: u64 = 300;
const MAX_RETRY_AFTER_SECS: u64 = 86_400;

/// The answer while an alias is paused after a 429: its last good reading
/// (with its own old time), else an error that says how long to wait.
fn paused_answer(alias: &str, left_ms: i64) -> Result<UsageInfo, UsageError> {
    if let Some(last) = crate::cache::last_good(alias) {
        return Ok(last);
    }
    let seconds = (left_ms + 999) / 1000;
    Err(UsageError {
        summary: format!("rate limited; retry in {seconds}s"),
        detail: format!("Claude usage: rate limited, next request allowed in {seconds}s"),
    })
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
