use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub pct: f64,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelWindow {
    pub name: String,
    pub pct: f64,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spend {
    pub used: f64,
    pub limit: f64,
    pub pct: f64,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeUsage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub models: Vec<ModelWindow>,
    pub spend: Option<Spend>,
}

pub fn parse_usage(v: &Value) -> Option<ClaudeUsage> {
    let window = |value: &Value| {
        Some(Window {
            pct: value.get("utilization")?.as_f64()?,
            resets_at: reset_time(value),
        })
    };
    let five_hour = v.get("five_hour").and_then(window);
    let seven_day = v.get("seven_day").and_then(window);
    let models: Vec<_> = v
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|limit| {
            Some(ModelWindow {
                name: limit
                    .pointer("/scope/model/display_name")?
                    .as_str()?
                    .to_owned(),
                pct: limit.get("percent")?.as_f64()?,
                resets_at: reset_time(limit),
            })
        })
        .collect();
    let spend = v.get("extra_usage").and_then(|extra| {
        if !extra.get("is_enabled")?.as_bool()? {
            return None;
        }
        Some(Spend {
            used: extra.get("used_credits")?.as_f64()? / 100.0,
            limit: extra.get("monthly_limit")?.as_f64()? / 100.0,
            pct: extra.get("utilization")?.as_f64()?,
            currency: extra.get("currency")?.as_str()?.to_owned(),
        })
    });
    if five_hour.is_none() && seven_day.is_none() && models.is_empty() && spend.is_none() {
        return None;
    }
    Some(ClaudeUsage {
        five_hour,
        seven_day,
        models,
        spend,
    })
}

fn reset_time(value: &Value) -> Option<String> {
    value
        .get("resets_at")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn now_ms() -> Result<i64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock precedes Unix epoch".to_owned())?;
    i64::try_from(elapsed.as_millis()).map_err(|_| "system clock exceeds timestamp range".to_owned())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoints {
    pub api_base: String,
    pub token_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api_base: "https://api.anthropic.com".to_owned(),
            token_url: "https://platform.claude.com/v1/oauth/token".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum UsageError {
    RateLimited { retry_after: Option<Duration> },
    Unauthorized,
    TokenExpired,
    Http(u16),
    Network(String),
    BadResponse(String),
}

pub async fn fetch_usage(
    client: &reqwest::Client,
    ep: &Endpoints,
    access_token: &str,
) -> Result<ClaudeUsage, UsageError> {
    let response = client
        .get(format!(
            "{}/api/oauth/usage",
            ep.api_base.trim_end_matches('/')
        ))
        .bearer_auth(access_token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header(
            "User-Agent",
            concat!("paper-claude-switch/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|error| UsageError::Network(error.to_string()))?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|header| header.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err(UsageError::RateLimited { retry_after });
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UsageError::Unauthorized);
    }
    if !status.is_success() {
        return Err(UsageError::Http(status.as_u16()));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| UsageError::Network(error.to_string()))?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|error| UsageError::BadResponse(format!("invalid usage JSON: {error}")))?;
    parse_usage(&body).ok_or_else(|| {
        UsageError::BadResponse("no usable usage windows or spend".to_owned())
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum RefreshOutcome {
    Refreshed(Value),
    Dead,
    ClientRejected,
    NoRefreshToken,
    Transient(String),
}

pub async fn refresh_oauth(
    client: &reqwest::Client,
    ep: &Endpoints,
    oauth: &Value,
) -> RefreshOutcome {
    let Some(refresh_token) = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
    else {
        return RefreshOutcome::NoRefreshToken;
    };
    let response = match client
        .post(&ep.token_url)
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return RefreshOutcome::Transient(error.to_string()),
    };
    let status = response.status();
    let body: Value = match response.json().await {
        Ok(body) => body,
        Err(_) => {
            return RefreshOutcome::Transient(format!("invalid token response (HTTP {status})"));
        }
    };
    if !status.is_success() {
        if matches!(status.as_u16(), 400 | 401 | 403) {
            match body.get("error").and_then(Value::as_str) {
                Some("invalid_grant") => return RefreshOutcome::Dead,
                Some("invalid_client") => return RefreshOutcome::ClientRejected,
                _ => {}
            }
        }
        return RefreshOutcome::Transient(format!("token endpoint HTTP {status}"));
    }
    let Some(access_token) = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
    else {
        return RefreshOutcome::Transient("missing access_token in token response".to_owned());
    };
    let expires_at = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|millis| now_ms().ok()?.checked_add(millis));
    let Some(expires_at) = expires_at else {
        return RefreshOutcome::Transient("invalid expires_in in token response".to_owned());
    };
    let Some(mut updated) = oauth.as_object().cloned() else {
        return RefreshOutcome::Transient("OAuth credentials must be an object".to_owned());
    };
    updated.insert("accessToken".to_owned(), json!(access_token));
    updated.insert("expiresAt".to_owned(), json!(expires_at));
    if let Some(token) = body.get("refresh_token") {
        if token.as_str().is_none_or(str::is_empty) {
            return RefreshOutcome::Transient("invalid refresh_token in token response".to_owned());
        }
        updated.insert("refreshToken".to_owned(), token.clone());
    }
    if let Some(scope) = body.get("scope") {
        let Some(scope) = scope.as_str() else {
            return RefreshOutcome::Transient("invalid scope in token response".to_owned());
        };
        updated.insert(
            "scopes".to_owned(),
            json!(scope.split(' ').filter(|part| !part.is_empty()).collect::<Vec<_>>()),
        );
    }
    RefreshOutcome::Refreshed(Value::Object(updated))
}

pub async fn usage_for_profile(
    client: &reqwest::Client,
    ep: &Endpoints,
    profile_dir: &Path,
    is_active: bool,
    live_oauth: Option<&Value>,
) -> Result<ClaudeUsage, UsageError> {
    let now = now_ms().map_err(UsageError::BadResponse)?;
    if is_active {
        // Claude Code owns this refresh lineage; never rotate it from here.
        let oauth = live_oauth.ok_or(UsageError::Unauthorized)?;
        if expires_at(oauth)? <= now {
            return Err(UsageError::TokenExpired);
        }
        return fetch_usage(client, ep, access_token(oauth)?).await;
    }

    let path = profile_dir.join("credentials.json");
    let bytes = std::fs::read(&path)
        .map_err(|error| UsageError::BadResponse(format!("reading profile credentials: {error}")))?;
    let mut credentials: Value = serde_json::from_slice(&bytes)
        .map_err(|error| UsageError::BadResponse(format!("invalid profile credentials JSON: {error}")))?;
    let mut oauth = credentials
        .get("claudeAiOauth")
        .cloned()
        .ok_or(UsageError::Unauthorized)?;
    if expires_at(&oauth)? <= now.saturating_add(300_000) {
        oauth = match refresh_oauth(client, ep, &oauth).await {
            RefreshOutcome::Refreshed(updated) => updated,
            RefreshOutcome::Dead | RefreshOutcome::NoRefreshToken => {
                return Err(UsageError::Unauthorized);
            }
            RefreshOutcome::ClientRejected => {
                return Err(UsageError::BadResponse("OAuth client rejected".to_owned()));
            }
            RefreshOutcome::Transient(message) => return Err(UsageError::Network(message)),
        };
        credentials["claudeAiOauth"] = oauth.clone();
        let bytes = serde_json::to_vec_pretty(&credentials)
            .map_err(|error| UsageError::BadResponse(format!("serializing profile credentials: {error}")))?;
        // Persist the rotated refresh token before any usage request can fail.
        crate::auth::atomic_write_private(&path, &bytes)
            .map_err(|error| UsageError::BadResponse(format!("persisting rotated credentials: {error}")))?;
    }
    fetch_usage(client, ep, access_token(&oauth)?).await
}

fn expires_at(oauth: &Value) -> Result<i64, UsageError> {
    oauth
        .get("expiresAt")
        .and_then(Value::as_i64)
        .ok_or_else(|| UsageError::BadResponse("missing or invalid expiresAt".to_owned()))
}

fn access_token(oauth: &Value) -> Result<&str, UsageError> {
    oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(UsageError::Unauthorized)
}
