use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::Error;
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::Serialize;

use crate::claude_usage::AccountInfo;
use crate::usage::{AdditionalRateLimit, UsageInfo, WindowUsage};

/// Marker error: the command already printed a user-facing failure message.
#[derive(Debug)]
pub struct OutputAlreadyReported;

impl std::fmt::Display for OutputAlreadyReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("command failed; details were already reported")
    }
}

impl std::error::Error for OutputAlreadyReported {}

pub(crate) fn should_report_error(error: &Error) -> bool {
    error.downcast_ref::<OutputAlreadyReported>().is_none()
}

// ── JSON types ───────────────────────────────────────────

#[derive(Serialize)]
pub struct JsonAccount {
    pub email: Option<String>,
    pub plan: Option<String>,
    pub account_id: Option<String>,
    pub workspace_name: Option<String>,
    /// End of the current paid period, unix seconds (as of the last token refresh)
    pub subscription_until: Option<i64>,
    /// "active", "canceled", "past_due", ... from the profile endpoint
    pub subscription_status: Option<String>,
}

#[derive(Serialize)]
pub struct JsonWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
    pub resets_in_seconds: Option<i64>,
    pub remaining_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pace_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub over_pace: Option<bool>,
}

#[derive(Serialize)]
pub struct JsonAdditionalLimit {
    pub limit_name: Option<String>,
    pub metered_feature: Option<String>,
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<Box<JsonWindow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary: Option<Box<JsonWindow>>,
}

/// Claude's "reset your session limit" offer.
#[derive(Serialize)]
pub struct JsonSessionReset {
    pub eligible: bool,
    pub available: bool,
    pub next_available_at: Option<String>,
    pub resets_per_week: u32,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum JsonUsage {
    Ok {
        fetched_at: String,
        primary: Option<Box<JsonWindow>>,
        secondary: Option<Box<JsonWindow>>,
        session_reset: Option<JsonSessionReset>,
        /// Usage-limit reset grants; absent when the reply had none.
        #[serde(skip_serializing_if = "Option::is_none")]
        reset_grants: Option<Vec<crate::claude_api::ResetGrant>>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        additional_limits: Vec<JsonAdditionalLimit>,
    },
    Err {
        error: String,
    },
}

#[derive(Serialize)]
pub struct JsonProfileWithUsage {
    pub alias: String,
    pub is_current: bool,
    pub account: JsonAccount,
    pub usage: JsonUsage,
}

#[derive(Serialize)]
pub struct JsonUsageResult {
    pub profiles: Vec<JsonProfileWithUsage>,
}

#[derive(Serialize)]
pub struct JsonBest {
    pub switched_to: String,
    pub account: JsonAccount,
    pub usage: JsonUsage,
    pub score: f64,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Serialize)]
pub struct JsonOk {
    pub ok: bool,
    pub alias: String,
    pub action: String,
}

#[derive(Serialize)]
pub struct JsonError {
    pub ok: bool,
    pub error: String,
}

// ── Conversion helpers ───────────────────────────────────

pub fn account_to_json(
    info: &AccountInfo,
    api_plan: Option<&str>,
    subscription_status: Option<&str>,
) -> JsonAccount {
    JsonAccount {
        subscription_status: subscription_status.map(str::to_owned),
        email: info.email.clone(),
        plan: api_plan
            .map(|s| s.to_string())
            .or_else(|| info.plan_type.clone()),
        account_id: info.account_id.clone(),
        workspace_name: info.workspace_name.clone(),
        subscription_until: info.subscription_until,
    }
}

fn window_to_json(w: &WindowUsage, label: &str, window_secs: i64) -> JsonWindow {
    let resets_in_seconds = w.resets_at.map(|ts| ts - crate::auth::now_unix_secs());
    let used = w.used_percent.unwrap_or(0.0);
    let pace = crate::usage::pace_percent(w, window_secs);
    JsonWindow {
        label: label.to_string(),
        used_percent: used,
        resets_at: w.resets_at,
        resets_in_seconds,
        remaining_percent: (100.0 - used).max(0.0),
        pace_percent: pace,
        over_pace: pace.map(|p| used > p),
    }
}

fn additional_limit_to_json(l: &AdditionalRateLimit) -> JsonAdditionalLimit {
    JsonAdditionalLimit {
        limit_name: l.limit_name.clone(),
        metered_feature: l.metered_feature.clone(),
        allowed: l.allowed,
        limit_reached: l.limit_reached,
        primary: l
            .primary
            .as_ref()
            .map(|w| Box::new(window_to_json(w, "5h", crate::usage::WINDOW_5H_SECS))),
        secondary: l
            .secondary
            .as_ref()
            .map(|w| Box::new(window_to_json(w, "7d", crate::usage::WINDOW_7D_SECS))),
    }
}

pub fn usage_to_json(result: Result<&UsageInfo, &str>) -> JsonUsage {
    match result {
        Err(e) => JsonUsage::Err {
            error: e.to_string(),
        },
        Ok(u) => {
            let fetched_at = u
                .fetched_at
                .map(format_iso8601)
                .unwrap_or_else(|| Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
            JsonUsage::Ok {
                fetched_at,
                primary: u
                    .primary
                    .as_ref()
                    .map(|w| Box::new(window_to_json(w, "5h", crate::usage::WINDOW_5H_SECS))),
                secondary: u
                    .secondary
                    .as_ref()
                    .map(|w| Box::new(window_to_json(w, "7d", crate::usage::WINDOW_7D_SECS))),
                session_reset: u.session_reset.as_ref().map(|reset| JsonSessionReset {
                    eligible: reset.eligible,
                    available: reset.available,
                    next_available_at: reset.next_available_at.clone(),
                    resets_per_week: reset.resets_per_week,
                }),
                reset_grants: u.reset_grants.clone(),
                additional_limits: u
                    .additional_limits
                    .iter()
                    .map(additional_limit_to_json)
                    .collect(),
            }
        }
    }
}

pub fn format_iso8601(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

/// Shared timestamp formatter: "2h30m (14:30)" or "1d12h (03-27 14:30)"
pub fn format_reset_time(ts: i64) -> String {
    let now = Local::now();
    let dt: DateTime<Local> = match Local.timestamp_opt(ts, 0).single() {
        Some(d) => d,
        None => return "--".into(),
    };
    if dt <= now {
        return "expired".into();
    }
    let secs = (dt - now).num_seconds().max(0) as u64;
    let relative = if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    };
    let local_fmt = if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%m-%d %H:%M").to_string()
    };
    format!("{relative} ({local_fmt})")
}

/// Short reset time for table columns: "14:30" or "03-27 14:30"
pub fn format_reset_short(ts: i64) -> String {
    let now = Local::now();
    let dt: DateTime<Local> = match Local.timestamp_opt(ts, 0).single() {
        Some(d) => d,
        None => return "--".into(),
    };
    if dt <= now {
        return "reset".into();
    }
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%m-%d %H:%M").to_string()
    }
}

/// Format a timestamp as local time: "HH:MM" (today) or "MM-DD HH:MM" (other days).
pub fn format_local_time(ts: i64) -> String {
    let now = Local::now();
    let dt: DateTime<Local> = match Local.timestamp_opt(ts, 0).single() {
        Some(d) => d,
        None => return "--".into(),
    };
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%m-%d %H:%M").to_string()
    }
}

/// Full local timestamp for detail views. The UTC offset keeps the value
/// unambiguous when screenshots or logs cross time zones.
pub fn format_local_timestamp(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M %:z").to_string())
        .unwrap_or_else(|| "--".into())
}

/// Format a token expiry with an explicit state so past JWT `exp` values are
/// never presented as a future expiration.
pub fn format_token_expiry(ts: i64) -> String {
    let Some(dt) = Local.timestamp_opt(ts, 0).single() else {
        return "not reported".into();
    };
    let timestamp = dt.format("%Y-%m-%d %H:%M %:z");
    if dt <= Local::now() {
        format!("expired {timestamp}")
    } else {
        format!("expires {timestamp}")
    }
}

// ── Output ───────────────────────────────────────────────

static JSON_PRETTY: OnceLock<bool> = OnceLock::new();

// 0 = Stdout, 1 = Stderr, 2 = Silent
static MESSAGE_MODE: AtomicU8 = AtomicU8::new(0);

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum MessageMode {
    Stdout = 0,
    Stderr = 1,
    Silent = 2,
}

/// Set JSON output mode. Call once at startup.
pub fn set_json_pretty(pretty: bool) {
    let _ = JSON_PRETTY.set(pretty);
}

pub fn set_message_mode(mode: MessageMode) {
    MESSAGE_MODE.store(mode as u8, Ordering::Relaxed);
}

fn is_pretty() -> bool {
    *JSON_PRETTY.get().unwrap_or(&false)
}

fn message_mode() -> MessageMode {
    match MESSAGE_MODE.load(Ordering::Relaxed) {
        1 => MessageMode::Stderr,
        2 => MessageMode::Silent,
        _ => MessageMode::Stdout,
    }
}

fn serialize<T: serde::Serialize>(val: &T) -> String {
    if is_pretty() {
        serde_json::to_string_pretty(val)
    } else {
        serde_json::to_string(val)
    }
    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

pub fn print_json<T: serde::Serialize>(val: &T) {
    println!("{}", serialize(val));
}

pub fn print_error(msg: &str) {
    let e = JsonError {
        ok: false,
        error: msg.to_string(),
    };
    println!("{}", serialize(&e));
}

pub fn user_println(msg: &str) {
    match message_mode() {
        MessageMode::Stdout => println!("{msg}"),
        MessageMode::Stderr => eprintln!("{msg}"),
        MessageMode::Silent => {}
    }
}

pub struct ProgressReporter {
    enabled: bool,
    label: String,
    total: usize,
    last_width: usize,
}

impl ProgressReporter {
    pub fn new(label: &str, total: usize) -> Self {
        let enabled = progress_enabled() && total > 0;
        let mut reporter = Self {
            enabled,
            label: label.to_string(),
            total,
            last_width: 0,
        };
        if reporter.enabled {
            reporter.advance(0);
        }
        reporter
    }

    pub fn advance(&mut self, completed: usize) {
        if !self.enabled {
            return;
        }

        let line = render_progress_line(&self.label, completed.min(self.total), self.total);
        self.last_width = line.chars().count();
        eprint!("\r{line}");
        let _ = io::stderr().flush();
    }

    pub fn finish(&mut self) {
        if !self.enabled {
            return;
        }

        eprint!("\r{}\r", " ".repeat(self.last_width.max(1)));
        let _ = io::stderr().flush();
        self.enabled = false;
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        self.finish();
    }
}

pub fn render_progress_line(label: &str, completed: usize, total: usize) -> String {
    let total = total.max(1);
    let completed = completed.min(total);
    let width = 24usize;
    let filled = completed.saturating_mul(width) / total;
    let bar = format!(
        "{}{}",
        "#".repeat(filled),
        "-".repeat(width.saturating_sub(filled))
    );
    format!("{label} [{bar}] {completed}/{total}")
}

fn progress_enabled() -> bool {
    if matches!(message_mode(), MessageMode::Silent) {
        return false;
    }
    if std::env::var("CS_PROGRESS_FORCE").ok().as_deref() == Some("1") {
        return true;
    }
    io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_reported_errors_are_not_reported_twice() {
        assert!(!should_report_error(&OutputAlreadyReported.into()));
        assert!(should_report_error(&anyhow::anyhow!("new failure")));
    }

    #[test]
    fn local_timestamp_includes_date_time_and_system_offset() {
        let rendered = format_local_timestamp(1_783_857_600);
        let expected = Local
            .timestamp_opt(1_783_857_600, 0)
            .single()
            .unwrap()
            .format("%Y-%m-%d %H:%M %:z")
            .to_string();

        assert_eq!(rendered, expected);
        assert!(!rendered.ends_with('Z'));
    }

    #[test]
    fn token_expiry_marks_past_timestamps_as_expired() {
        let text = format_token_expiry(crate::auth::now_unix_secs() - 60);
        assert!(text.starts_with("expired "));
    }
}
