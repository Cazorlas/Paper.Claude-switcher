use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;
use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};
use crate::http_retry::{self, ReplaySafety};
const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";

fn models_url() -> String {
    std::env::var("CS_MODELS_URL").unwrap_or_else(|_| MODELS_URL.to_string())
}

fn build_models_request(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    version: &str,
) -> reqwest::RequestBuilder {
    crate::usage::apply_account_routing_headers(
        client
            .get(models_url())
            .query(&[("client_version", version)])
            .bearer_auth(access_token),
        account_id,
        is_fedramp,
    )
}

/// One entry from the `/models` endpoint's `models[]` array.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelEntry {
    pub slug: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub visibility: Option<String>,
    pub priority: Option<i64>,
    pub supported_in_api: Option<bool>,
    pub context_window: Option<u64>,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub input_modalities: Vec<String>,
    pub additional_speed_tiers: Vec<String>,
    pub service_tiers: Vec<String>,
    pub default_service_tier: Option<String>,
    pub max_context_window: Option<u64>,
    pub auto_compact_token_limit: Option<u64>,
    pub effective_context_window_percent: Option<i64>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub supports_image_detail_original: Option<bool>,
    pub experimental_supported_tools: Vec<String>,
    pub supports_search_tool: Option<bool>,
    pub use_responses_lite: Option<bool>,
}

/// Parse the `/models` endpoint's JSON body into a `Vec<ModelEntry>`. Entries
/// missing a `slug` are skipped; other fields are treated as optional
/// (defensively ignoring unknown fields per the upstream contract).
fn parse_models_body(body: &serde_json::Value) -> Result<Vec<ModelEntry>> {
    let models = body["models"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no models array in response"))?;

    let mut seen = HashSet::new();
    Ok(models
        .iter()
        .filter_map(|m| {
            let slug = m["slug"].as_str()?.trim().to_string();
            if slug.is_empty() || !seen.insert(slug.clone()) {
                return None;
            }
            let string_list = |key: &str| {
                m.get(key)
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            Some(ModelEntry {
                slug,
                display_name: m["display_name"].as_str().map(String::from),
                description: m["description"].as_str().map(String::from),
                visibility: m["visibility"].as_str().map(String::from),
                priority: m["priority"].as_i64(),
                supported_in_api: m["supported_in_api"].as_bool(),
                context_window: m["context_window"].as_u64(),
                default_reasoning_effort: m["default_reasoning_level"]
                    .as_str()
                    .or_else(|| m["default_reasoning_effort"].as_str())
                    .map(String::from),
                supported_reasoning_efforts: m
                    .get("supported_reasoning_levels")
                    .or_else(|| m.get("supported_reasoning_efforts"))
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                item.as_str()
                                    .or_else(|| item.get("effort").and_then(|v| v.as_str()))
                                    .or_else(|| {
                                        item.get("reasoning_effort").and_then(|v| v.as_str())
                                    })
                                    .map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                input_modalities: string_list("input_modalities"),
                additional_speed_tiers: string_list("additional_speed_tiers"),
                service_tiers: m
                    .get("service_tiers")
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                item.as_str()
                                    .or_else(|| item.get("id").and_then(|v| v.as_str()))
                                    .map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                default_service_tier: m["default_service_tier"].as_str().map(String::from),
                max_context_window: m["max_context_window"].as_u64(),
                auto_compact_token_limit: m["auto_compact_token_limit"].as_u64(),
                effective_context_window_percent: m["effective_context_window_percent"].as_i64(),
                supports_parallel_tool_calls: m["supports_parallel_tool_calls"].as_bool(),
                supports_image_detail_original: m["supports_image_detail_original"].as_bool(),
                experimental_supported_tools: string_list("experimental_supported_tools"),
                supports_search_tool: m["supports_search_tool"].as_bool(),
                use_responses_lite: m["use_responses_lite"].as_bool(),
            })
        })
        .collect())
}

/// Sort models for display: ascending priority (lowest number first), unknown
/// priority sorts last. Does not filter hidden models — callers decide how to
/// present `visibility == "hide"` entries (e.g. dim them rather than drop them).
pub(crate) fn sorted_models_for_display(models: &[ModelEntry]) -> Vec<&ModelEntry> {
    let mut sorted: Vec<&ModelEntry> = models.iter().collect();
    sorted.sort_by_key(|m| m.priority.unwrap_or(i64::MAX));
    sorted
}

/// Fetch and parse the full model list from the `/models` endpoint.
pub(crate) async fn fetch_models(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
) -> Result<Vec<ModelEntry>> {
    let version = crate::auth::codex_cli_version();
    let started = Instant::now();
    for attempt in 1..=3 {
        let response = http_retry::send(
            build_models_request(client, access_token, account_id, is_fedramp, version),
            ReplaySafety::Idempotent,
        )
        .await;
        match response {
            Ok(resp) if resp.status.is_success() => {
                let body: serde_json::Value =
                    serde_json::from_slice(&resp.body).map_err(|error| {
                        info!(
                            status = resp.status.as_u16(),
                            client_version = version,
                            is_fedramp,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            outcome = "invalid_json",
                            "authenticated /models response is not valid JSON"
                        );
                        anyhow::Error::new(error).context(format!(
                            "decoding /models response for Codex client_version {version}"
                        ))
                    })?;
                let models = parse_models_body(&body).map_err(|error| {
                    info!(
                        status = resp.status.as_u16(),
                        client_version = version,
                        is_fedramp,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        outcome = "invalid_catalog",
                        "authenticated /models catalog could not be parsed"
                    );
                    error.context(format!(
                        "parsing /models catalog for Codex client_version {version}"
                    ))
                })?;
                if models.is_empty() {
                    info!(
                        model_count = 0,
                        status = resp.status.as_u16(),
                        client_version = version,
                        is_fedramp,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        outcome = "empty_catalog",
                        "authenticated /models catalog returned no models"
                    );
                    return Ok(models);
                }
                info!(
                    model_count = models.len(),
                    status = resp.status.as_u16(),
                    client_version = version,
                    is_fedramp,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    outcome = "ok",
                    "fetched authenticated /models catalog"
                );
                return Ok(models);
            }
            Ok(resp) => {
                let status = resp.status;
                let retryable = status.is_server_error();
                if !retryable || attempt == 3 {
                    info!(
                        status = status.as_u16(),
                        client_version = version,
                        is_fedramp,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        outcome = "http_error",
                        "authenticated /models catalog request failed"
                    );
                    return Err(anyhow::Error::new(ModelsHttpError(status)).context(format!(
                        "/models request used Codex client_version {version}"
                    )));
                }
                debug!("models fetch attempt {attempt}/3 returned {status}; retrying");
            }
            Err(error) => {
                if attempt == 3 {
                    info!(
                        client_version = version,
                        is_fedramp,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        outcome = "transport_error",
                        "authenticated /models catalog request failed"
                    );
                    return Err(error.context(format!(
                        "models fetch failed after 3 attempts with Codex client_version {version}"
                    )));
                }
                debug!("models fetch attempt {attempt}/3 failed: {error}; retrying");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250 * attempt)).await;
    }
    unreachable!("models fetch loop always returns")
}

#[derive(Debug)]
struct ModelsHttpError(reqwest::StatusCode);

impl std::fmt::Display for ModelsHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "models endpoint returned {}", self.0)
    }
}

impl std::error::Error for ModelsHttpError {}

fn is_models_auth_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ModelsHttpError>()
        .is_some_and(|error| error.0 == reqwest::StatusCode::UNAUTHORIZED)
}

fn require_profile_model_catalog(
    alias: &str,
    models: Vec<ModelEntry>,
    client_version: &str,
) -> Result<Vec<ModelEntry>> {
    if models.is_empty() {
        bail!(
            "{alias}: authenticated /models catalog returned no models for Codex client_version {client_version}"
        );
    }
    Ok(models)
}

/// Fetch the full model list for a profile (for display, e.g. the TUI detail
/// panel). Unlike `warmup_account`, this never sends a warmup ping — it only
/// refreshes an expiring access token before calling the `/models` endpoint.
pub(crate) async fn fetch_models_for_profile(
    alias: &str,
    profile_path: &Path,
) -> Result<Vec<ModelEntry>> {
    crate::auth::ensure_chatgpt_backend_supported(&format!(
        "fetch models for ChatGPT profile '{alias}'"
    ))?;
    let val = crate::auth::read_auth(profile_path)
        .map_err(|e| anyhow::anyhow!("{alias}: cannot read auth: {e}"))?;

    let (at, rt) = crate::auth::extract_tokens(&val);
    let info = crate::jwt::parse_account_info(&val);
    let mut profile_tokens = crate::usage::ProfileTokens {
        id_token: crate::auth::extract_id_token(&val),
        access_token: at
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{alias}: no access_token in profile"))?,
        refresh_token: rt.filter(|s| !s.is_empty()),
        account_id: info.account_id,
        email: info.email.map(|email| email.to_lowercase()),
        is_fedramp: info.is_fedramp,
    };

    let client = crate::auth::build_http_client()?;
    let mut refresh_attempted = false;

    if profile_tokens.refresh_token.is_some()
        && crate::jwt::is_token_expiring(&profile_tokens.access_token, 60) == Some(true)
    {
        match crate::usage::refresh_profile_tokens(alias, profile_path, &profile_tokens).await {
            Ok(refreshed) => {
                profile_tokens = refreshed;
                refresh_attempted = true;
            }
            Err(error) if crate::usage::is_refresh_safety_error(&error) => return Err(error),
            // Deliberate degrade: fall through and try /models with the
            // existing (possibly expiring) token rather than failing here.
            // Still worth a diagnosable trace — silently swallowing this
            // sent people chasing an unrelated /models error instead of the
            // real cause (a rejected/expired refresh_token).
            Err(e) => {
                if let Some(terminal) = e.downcast_ref::<crate::usage::TerminalAuthError>() {
                    warn!(
                        alias,
                        code = terminal.code,
                        "proactive token refresh rejected, continuing with existing token"
                    );
                    refresh_attempted = true;
                } else {
                    // Nothing was rotated, so the /models 401 recovery may
                    // still spend its single refresh.
                    warn!(
                        "[{alias}] proactive token refresh failed, continuing with existing token"
                    );
                }
            }
        }
    }

    let models = match fetch_models(
        &client,
        &profile_tokens.access_token,
        profile_tokens.account_id.as_deref(),
        profile_tokens.is_fedramp,
    )
    .await
    {
        Ok(models) => models,
        Err(error) if is_models_auth_error(&error) => {
            let expected = profile_tokens.clone();
            profile_tokens = if refresh_attempted {
                crate::usage::reload_profile_tokens_if_changed(alias, profile_path, &expected)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "{alias}: /models returned HTTP 401 after the token refresh attempt"
                        )
                    })?
            } else {
                crate::usage::refresh_profile_tokens(alias, profile_path, &expected)
                    .await
                    .with_context(|| format!("{alias}: recovering authentication for /models"))?
            };
            fetch_models(
                &client,
                &profile_tokens.access_token,
                profile_tokens.account_id.as_deref(),
                profile_tokens.is_fedramp,
            )
            .await?
        }
        Err(error) => return Err(error),
    };
    require_profile_model_catalog(alias, models, crate::auth::codex_cli_version())
}
