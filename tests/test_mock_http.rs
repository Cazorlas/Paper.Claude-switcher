//! HTTP-level integration tests using the mock server.
//!
//! These tests start a real HTTP mock server, create temp profile directories
//! with fake auth.json files, and call the mock directly via reqwest to verify
//! the HTTP → parse → score pipeline.

mod mock;

use claude_switch::auth;
use claude_switch::jwt::AccountInfo;
use claude_switch::usage::{self, ScoredCandidate};
use mock::scenarios;
use serde_json::json;
use std::path::PathBuf;

static HTTP_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: String) -> Self {
        let previous = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var(key).ok();
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// Create a temp directory with fake profile auth.json files.
/// Returns (temp_dir, vec of (alias, path, token, JWT account info)).
fn setup_profiles(
    entries: &[(String, Vec<serde_json::Value>)],
) -> (
    tempfile::TempDir,
    Vec<(String, PathBuf, String, AccountInfo)>,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut profiles = Vec::new();

    for (token, _responses) in entries {
        let alias = token.strip_prefix("tok_").unwrap_or(token).to_string();
        let profile_dir = dir.path().join(&alias);
        std::fs::create_dir_all(&profile_dir).unwrap();

        let auth_json = json!({
            "tokens": {
                "access_token": token,
                "refresh_token": format!("refresh_{token}"),
                "id_token": "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ0ZXN0In0.fake"
            }
        });
        let auth_path = profile_dir.join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::to_string_pretty(&auth_json).unwrap(),
        )
        .unwrap();

        profiles.push((alias, auth_path, token.clone(), AccountInfo::default()));
    }

    (dir, profiles)
}

/// Helper: fetch usage from mock, parse, build candidates, compute pool state, score, and rank.
/// Returns (alias, score) sorted best-first.
fn score_from_responses(
    responses: &[(String, serde_json::Value)],
    profiles: &[(String, PathBuf, String, AccountInfo)],
    team_priority: bool,
    safety_margin_7d: f64,
    now: i64,
) -> Vec<(String, f64)> {
    score_candidates_from_responses(responses, profiles, team_priority, safety_margin_7d, now)
        .into_iter()
        .map(|scored| (scored.candidate.alias, scored.score))
        .collect()
}

fn score_candidates_from_responses(
    responses: &[(String, serde_json::Value)],
    profiles: &[(String, PathBuf, String, AccountInfo)],
    team_priority: bool,
    safety_margin_7d: f64,
    now: i64,
) -> Vec<ScoredCandidate> {
    let inputs = responses
        .iter()
        .map(|(alias, body)| {
            let account = profiles
                .iter()
                .find(|(profile_alias, _, _, _)| profile_alias == alias)
                .unwrap()
                .3
                .clone();
            (alias.clone(), usage::parse_usage(body), account, 0)
        })
        .collect();
    let mut scored = usage::score_candidates(inputs, now, safety_margin_7d, team_priority);
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    scored
}

/// Fetch all profiles from mock server and return (alias, response_body) pairs.
async fn fetch_all(
    client: &reqwest::Client,
    url: &str,
    profiles: &[(String, PathBuf, String, AccountInfo)],
) -> Vec<(String, serde_json::Value)> {
    let mut results = Vec::new();
    for (alias, _path, token, _account) in profiles {
        let resp = client
            .get(url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "HTTP request for {alias} failed: {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        results.push((alias.clone(), body));
    }
    results
}

#[test]
fn score_candidates_pool_size_counts_successful_responses_not_profiles() {
    let entries = scenarios::healthy_pool();
    let (_dir, profiles) = setup_profiles(&entries);
    let responses: Vec<_> = entries
        .iter()
        .take(2)
        .map(|(token, bodies)| {
            (
                token.strip_prefix("tok_").unwrap_or(token).to_string(),
                bodies[0].clone(),
            )
        })
        .collect();

    let scored =
        score_candidates_from_responses(&responses, &profiles, false, 20.0, auth::now_unix_secs());

    assert_eq!(profiles.len(), 3);
    assert_eq!(scored.len(), 2);
    assert!(
        scored
            .iter()
            .all(|candidate| candidate.candidate.pool_size == 2)
    );
}

#[tokio::test]
async fn usage_401_refreshes_json_token_and_retries_with_new_access_token() {
    let _lock = HTTP_ENV_LOCK.lock().await;
    let refreshed_access_token = "mock_access_refresh_old";
    let server = mock::MockServer::start_programmed(vec![
        (
            "old_access".to_string(),
            vec![mock::MockResponse::json(
                reqwest::StatusCode::UNAUTHORIZED,
                json!({"error": "expired"}),
            )],
        ),
        (
            refreshed_access_token.to_string(),
            vec![mock::MockResponse::json(
                reqwest::StatusCode::OK,
                mock::transformer::base_response("plus", 12.0, 18000, 20.0, 604800),
            )],
        ),
    ])
    .await;
    let _usage_url = EnvVarGuard::set("CS_USAGE_URL", server.usage_url());
    let _token_url = EnvVarGuard::set("CS_TOKEN_URL", server.token_url());
    let _reset_url = EnvVarGuard::remove("CS_RESET_CREDITS_URL");

    let outcome = usage::fetch_usage_with_refresh(
        "refresh_case",
        "old_access",
        Some("old_id"),
        Some("refresh_old"),
        None,
        false,
    )
    .await;
    let usage = outcome.result.unwrap();

    assert_eq!(usage.primary.unwrap().used_percent, Some(12.0));
    assert_eq!(
        outcome.refreshed.unwrap().access_token,
        refreshed_access_token
    );
    assert_eq!(server.request_count("old_access"), 1);
    assert_eq!(server.request_count(refreshed_access_token), 1);
    server.shutdown();
}

#[tokio::test]
async fn usage_5xx_returns_contextual_error() {
    let _lock = HTTP_ENV_LOCK.lock().await;
    let server = mock::MockServer::start_programmed(vec![(
        "server_error".to_string(),
        vec![mock::MockResponse::text(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "upstream failed",
        )],
    )])
    .await;
    let _usage_url = EnvVarGuard::set("CS_USAGE_URL", server.usage_url());

    let error =
        usage::fetch_usage_with_refresh("server_error", "server_error", None, None, None, false)
            .await
            .result
            .expect_err("HTTP 500 must fail");

    assert!(error.to_string().contains("HTTP 500"), "{error:#}");
    server.shutdown();
}

#[tokio::test]
async fn usage_malformed_json_returns_parse_context() {
    let _lock = HTTP_ENV_LOCK.lock().await;
    let server = mock::MockServer::start_programmed(vec![(
        "malformed".to_string(),
        vec![mock::MockResponse::text(
            reqwest::StatusCode::OK,
            "not-json",
        )],
    )])
    .await;
    let _usage_url = EnvVarGuard::set("CS_USAGE_URL", server.usage_url());

    let error = usage::fetch_usage_with_refresh("malformed", "malformed", None, None, None, false)
        .await
        .result
        .expect_err("malformed JSON must fail");

    assert!(
        error
            .to_string()
            .contains("failed to parse usage response (HTTP 200 OK)"),
        "{error:#}"
    );
    server.shutdown();
}

#[tokio::test]
async fn usage_retry_exhaustion_returns_last_error_after_three_attempts() {
    let _lock = HTTP_ENV_LOCK.lock().await;
    let server = mock::MockServer::start_programmed(vec![(
        "retry_exhausted".to_string(),
        vec![mock::MockResponse::text(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "still unavailable",
        )],
    )])
    .await;
    let _usage_url = EnvVarGuard::set("CS_USAGE_URL", server.usage_url());
    let dir = tempfile::tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    std::fs::write(
        &auth_path,
        serde_json::to_vec(&json!({
            "tokens": {"access_token": "retry_exhausted"}
        }))
        .unwrap(),
    )
    .unwrap();

    let error = usage::fetch_usage_retried_force("retry_exhausted", &auth_path, "")
        .await
        .unwrap_err();

    assert!(error.detail.contains("HTTP 503"), "{}", error.detail);
    assert_eq!(server.request_count("retry_exhausted"), 3);
    server.shutdown();
}

// ── Tests ──

#[tokio::test]
async fn http_healthy_pool_ranking() {
    let entries = scenarios::healthy_pool();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    let responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    let scored = score_from_responses(&responses, &profiles, true, 20.0, now);

    assert_eq!(scored[0].0, "healthy_a", "0% used should rank first");
    assert_eq!(scored[2].0, "healthy_c", "60% used should rank last");

    // Verify scores are in the usable tier
    for (alias, score) in &scored {
        assert!(
            *score > 1000.0,
            "{alias} should be in usable tier, got {score}"
        );
    }

    server.shutdown();
}

#[tokio::test]
async fn http_team_priority() {
    let entries = scenarios::team_priority();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    let responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    let scored = score_from_responses(&responses, &profiles, true, 20.0, now);

    assert_eq!(
        scored[0].0, "team",
        "team should rank first with +500 bonus"
    );
    // Team score should be 500+ higher than plus accounts
    assert!(
        scored[0].1 - scored[1].1 > 400.0,
        "team bonus should create large gap"
    );

    server.shutdown();
}

#[tokio::test]
async fn http_drain_window() {
    let entries = scenarios::drain_window();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    let responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    let scored = score_from_responses(&responses, &profiles, false, 20.0, now);

    assert_eq!(
        scored[0].0, "drain_a",
        "20min-to-reset should be drained first"
    );

    server.shutdown();
}

#[tokio::test]
async fn http_seven_day_crisis() {
    let entries = scenarios::seven_day_crisis();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    let responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    let scored = score_from_responses(&responses, &profiles, false, 20.0, now);

    assert_eq!(
        scored[0].0, "7d_crisis_b",
        "healthy 7d should outrank 95% 7d"
    );

    server.shutdown();
}

#[tokio::test]
async fn http_all_exhausted() {
    let entries = scenarios::all_exhausted();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    let responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    // pool_exhausted is computed dynamically inside score_from_responses
    let scored = score_from_responses(&responses, &profiles, false, 20.0, now);

    assert_eq!(
        scored[0].0, "exhausted_a",
        "soonest reset (30min) should rank first"
    );
    assert!(
        scored[0].1 < 500.0,
        "exhausted accounts should be in low tier"
    );

    server.shutdown();
}

#[tokio::test]
async fn http_timeline_gradual_exhaustion() {
    let entries = scenarios::gradual_exhaustion();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    // Tick 0: A=30%, B=20% — both healthy
    let tick0_responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    let tick0 = score_from_responses(&tick0_responses, &profiles, false, 20.0, now);
    for (alias, score) in &tick0 {
        assert!(
            *score > 900.0,
            "{alias} should be usable at tick 0, got {score}"
        );
    }

    // Tick 1: A=60%, B=20%
    // Tick 2: A=90%, B=20%
    // Advance cursors by fetching 2 more times per account
    for _ in 0..2 {
        for (_alias, _path, token, _) in &profiles {
            let _ = client
                .get(server.usage_url())
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .unwrap();
        }
    }

    // Tick 3: A=100%, B=20% — A exhausted, B should win
    let tick3_responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    // pool_exhausted is computed dynamically
    let tick3 = score_from_responses(&tick3_responses, &profiles, false, 20.0, now);

    assert_eq!(
        tick3[0].0, "gradual_b",
        "B should win when A is exhausted at tick 3"
    );
    assert!(tick3[1].1 < 500.0, "exhausted A should score low");

    server.shutdown();
}

#[tokio::test]
async fn http_mock_returns_correct_structure() {
    // Verify that the mock response is parseable by the real parse_usage
    let entries = scenarios::healthy_pool();
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(server.usage_url())
        .header("Authorization", "Bearer tok_healthy_a")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();

    // Verify structure matches real API
    assert!(body.get("plan_type").is_some(), "should have plan_type");
    assert!(body.get("rate_limit").is_some(), "should have rate_limit");
    assert!(
        body.pointer("/rate_limit/primary_window/used_percent")
            .is_some()
    );
    assert!(
        body.pointer("/rate_limit/primary_window/reset_at")
            .is_some()
    );
    assert!(
        body.pointer("/rate_limit/secondary_window/used_percent")
            .is_some()
    );
    assert!(
        body.pointer("/rate_limit/secondary_window/reset_at")
            .is_some()
    );
    assert!(body.get("credits").is_some(), "should have credits");

    // Parse through the real path
    let info = usage::parse_usage(&body);
    assert!(info.primary.is_some(), "should parse primary window");
    assert!(info.secondary.is_some(), "should parse secondary window");
    assert_eq!(info.primary.as_ref().unwrap().used_percent, Some(0.0));

    server.shutdown();
}

#[tokio::test]
async fn http_unknown_token_returns_401() {
    let entries = scenarios::healthy_pool();
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(server.usage_url())
        .header("Authorization", "Bearer unknown_token")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401, "unknown token should get 401");

    server.shutdown();
}
