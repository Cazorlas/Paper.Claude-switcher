use serde::{Deserialize, Serialize};

mod scoring;

pub use scoring::{
    is_available, is_candidate_eligible, pace_percent, score_candidates, visible_pace_percent,
};
#[allow(unused_imports)]
pub use scoring::score_unified;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WindowUsage {
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub window_minutes: Option<i64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SpendControlLimit {
    pub source: Option<String>,
    pub limit: Option<String>,
    pub used: Option<String>,
    pub remaining: Option<String>,
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<i64>,
}

/// One extra rate-limit pool (for example a per-model weekly limit) with its
/// own independent windows.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AdditionalRateLimit {
    pub limit_name: Option<String>,
    pub metered_feature: Option<String>,
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub primary: Option<WindowUsage>,
    pub secondary: Option<WindowUsage>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ResetCredit {
    pub id: String,
    pub granted_at: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct UsageInfo {
    pub fetched_at: Option<i64>,
    pub primary: Option<WindowUsage>,   // 5h window
    pub secondary: Option<WindowUsage>, // 7d window
    pub credits_balance: Option<f64>,
    pub unlimited_credits: Option<bool>,
    /// plan type reported with the usage (authoritative when present)
    pub plan_type: Option<String>,
    pub reset_credits_available_count: Option<u64>,
    pub reset_credits: Vec<ResetCredit>,
    pub reset_credits_error: Option<String>,
    /// Explicit account-level restriction reported by the API.
    pub account_limited: bool,
    /// Backend-classified limit reason, preserved for detailed diagnostics.
    pub rate_limit_reached_type: Option<String>,
    /// Effective spend-control limit, when supplied by the backend.
    pub individual_limit: Option<Box<SpendControlLimit>>,
    /// Per-model rate limits.
    pub additional_limits: Vec<AdditionalRateLimit>,
    /// Subscription status from the profile endpoint ("active", "canceled",
    /// "past_due", ...), when known.
    pub subscription_status: Option<String>,
}

/// One assembled display row for an additional-limit pool. Pure data,
/// derived from `AdditionalRateLimit` so CLI and TUI renderers can share
/// the same assembly logic instead of each re-deriving `unavailable`.
#[derive(Debug, Clone)]
pub struct PoolRow {
    pub limit_name: String,
    /// True when the API reports the pool as exhausted or disallowed.
    pub unavailable: bool,
    pub primary: Option<WindowUsage>,
    pub secondary: Option<WindowUsage>,
}

/// Assemble display rows from the raw `additional_limits` array. Returns an
/// empty vec when there are no additional pools.
pub fn additional_pool_rows(limits: &[AdditionalRateLimit]) -> Vec<PoolRow> {
    limits
        .iter()
        .map(|l| PoolRow {
            limit_name: l.limit_name.clone().unwrap_or_else(|| "pool".to_string()),
            unavailable: l.limit_reached == Some(true) || l.allowed == Some(false),
            primary: l.primary.clone(),
            secondary: l.secondary.clone(),
        })
        .collect()
}

/// All data needed to score an account. Pure data, no I/O.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub alias: String,
    pub used_5h: f64,
    pub resets_at_5h: Option<i64>,
    pub used_7d: f64,
    pub resets_at_7d: Option<i64>,
    pub has_5h_data: bool,
    pub has_7d_data: bool,
    pub is_team: bool,
    pub is_free: bool,
    pub last_used: i64,
    pub now: i64,
    // Pool-level signals (set by caller after building all candidates)
    pub pool_size: usize,
    pub pool_exhausted: usize,
    pub team_priority: bool,
}

impl Candidate {
    /// Build from UsageInfo + metadata. `now` should be shared across all candidates.
    pub fn from_usage(
        alias: String,
        u: &UsageInfo,
        is_team: bool,
        is_free: bool,
        last_used: i64,
        now: i64,
    ) -> Self {
        let force_exhausted = u.account_limited;
        Self {
            alias,
            used_5h: if force_exhausted {
                100.0
            } else {
                u.primary
                    .as_ref()
                    .and_then(|w| w.used_percent)
                    .unwrap_or(0.0)
            },
            resets_at_5h: (!force_exhausted)
                .then(|| u.primary.as_ref().and_then(|w| w.resets_at))
                .flatten(),
            used_7d: if force_exhausted {
                100.0
            } else {
                u.secondary
                    .as_ref()
                    .and_then(|w| w.used_percent)
                    .unwrap_or(0.0)
            },
            resets_at_7d: (!force_exhausted)
                .then(|| u.secondary.as_ref().and_then(|w| w.resets_at))
                .flatten(),
            has_5h_data: u.primary.is_some() || force_exhausted,
            has_7d_data: u.secondary.is_some() || force_exhausted,
            is_team,
            is_free,
            last_used,
            now,
            pool_size: 1,
            pool_exhausted: 0,
            team_priority: false,
        }
    }

    /// Reset-aware effective 5h usage: 0.0 if window has already reset.
    pub fn effective_used_5h(&self) -> f64 {
        if self.resets_at_5h.is_some_and(|ts| ts <= self.now) {
            0.0
        } else {
            self.used_5h
        }
    }

    /// Reset-aware effective 7d usage: 0.0 if window has already reset.
    pub fn effective_used_7d(&self) -> f64 {
        if self.resets_at_7d.is_some_and(|ts| ts <= self.now) {
            0.0
        } else {
            self.used_7d
        }
    }
}

/// Window durations in seconds (used for pace calculation).
pub const WINDOW_5H_SECS: i64 = 5 * 3600;
pub const WINDOW_7D_SECS: i64 = 7 * 86400;

/// Free plan accounts become ineligible below this 5h remaining%.
pub const FREE_FLOOR_PCT: f64 = 35.0;

/// How much of the cache a usage fetch may skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Serve a fresh cache entry as-is. Everyday reads.
    Cached,
    /// Ignore the usage TTL. What a timer with nobody watching wants.
    Unattended,
    /// A person explicitly asking again.
    Forced,
}

/// Claude usage for a saved profile, served from the usage cache when fresh.
/// The active account is read with its live token and never refreshed here.
pub async fn fetch_usage_retried(alias: &str) -> std::result::Result<UsageInfo, UsageError> {
    crate::claude_usage::fetch_alias(alias, false).await
}

/// Bypass the usage TTL for current numbers. Used by background refreshes.
pub async fn fetch_usage_retried_unattended(
    alias: &str,
) -> std::result::Result<UsageInfo, UsageError> {
    crate::claude_usage::fetch_alias(alias, true).await
}

/// Bypass every cache. For a person explicitly asking again, or a decision
/// that must not run on stale numbers.
pub async fn fetch_usage_retried_force(
    alias: &str,
) -> std::result::Result<UsageInfo, UsageError> {
    crate::claude_usage::fetch_alias(alias, true).await
}

/// Structured error for usage fetch failures.
#[derive(Debug, Clone)]
pub struct UsageError {
    /// Short summary for user-facing display (e.g. "HTTP 401 Unauthorized")
    pub summary: String,
    /// Full detail for debug/log
    pub detail: String,
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

/// One scored candidate. Pure data, no I/O.
pub struct ScoredCandidate {
    pub candidate: Candidate,
    pub usage: UsageInfo,
    pub score: f64,
}

#[cfg(test)]
mod pool_row_tests {
    use super::*;

    #[test]
    fn empty_additional_limits_yields_no_rows() {
        assert!(additional_pool_rows(&[]).is_empty());
    }

    #[test]
    fn pool_with_both_windows_produces_one_row() {
        let limits = vec![AdditionalRateLimit {
            limit_name: Some("Sonnet".to_string()),
            metered_feature: None,
            allowed: Some(true),
            limit_reached: Some(false),
            primary: Some(WindowUsage {
                used_percent: Some(42.0),
                resets_at: Some(1000),
                window_minutes: Some(300),
            }),
            secondary: Some(WindowUsage {
                used_percent: Some(10.0),
                resets_at: Some(2000),
                window_minutes: Some(10_080),
            }),
        }];

        let rows = additional_pool_rows(&limits);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].limit_name, "Sonnet");
        assert!(!rows[0].unavailable);
        assert_eq!(rows[0].primary.as_ref().unwrap().used_percent, Some(42.0));
        assert_eq!(rows[0].secondary.as_ref().unwrap().used_percent, Some(10.0));
    }

    #[test]
    fn limit_reached_pool_is_marked_unavailable() {
        let limits = vec![AdditionalRateLimit {
            limit_name: Some("exhausted-pool".to_string()),
            limit_reached: Some(true),
            allowed: Some(true),
            ..Default::default()
        }];

        let rows = additional_pool_rows(&limits);
        assert!(rows[0].unavailable);
    }

    #[test]
    fn disallowed_pool_is_marked_unavailable() {
        let limits = vec![AdditionalRateLimit {
            limit_name: Some("disallowed-pool".to_string()),
            allowed: Some(false),
            limit_reached: Some(false),
            ..Default::default()
        }];

        let rows = additional_pool_rows(&limits);
        assert!(rows[0].unavailable);
    }
}
