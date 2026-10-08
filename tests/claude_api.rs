mod mock;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use claude_switch::claude_api::{
    ClaudeUsage, ModelWindow, RefreshOutcome, Spend, UsageError, Window,
    fetch_usage, parse_usage, refresh_oauth, usage_for_profile,
};
use mock::{MockResponse, claude_api::ClaudeServer};
use serde_json::{Value, json};

fn row_one() -> Value {
    json!({
        "five_hour": {"utilization": 25.0, "resets_at": "2026-06-22T23:29:59Z"},
        "seven_day": {"utilization": 16.0, "resets_at": "2026-06-26T17:59:59Z"},
        "limits": [
            {"scope": {"model": {"display_name": "Fable"}}, "percent": 100,
             "resets_at": "2026-06-26T17:59:59Z"},
            {"scope": {}, "percent": 5}
        ]
    })
}

fn expected_usage() -> ClaudeUsage {
    ClaudeUsage {
        five_hour: Some(Window {
            pct: 25.0,
            resets_at: Some("2026-06-22T23:29:59Z".into()),
        }),
        seven_day: Some(Window {
            pct: 16.0,
            resets_at: Some("2026-06-26T17:59:59Z".into()),
        }),
        models: vec![ModelWindow {
            name: "Fable".into(),
            pct: 100.0,
            resets_at: Some("2026-06-26T17:59:59Z".into()),
        }],
        spend: None,
        session_reset: None,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

fn oauth(expires_at: i64) -> Value {
    json!({"accessToken": "old", "refreshToken": "rt1", "expiresAt": expires_at,
           "subscriptionType": "max"})
}

fn rotation() -> Value {
    json!({"access_token": "new", "expires_in": 3600, "refresh_token": "rt2",
           "scope": "user:inference user:profile"})
}

async fn server(token_status: StatusCode, token_body: Value) -> ClaudeServer {
    ClaudeServer::start(
        MockResponse::json(StatusCode::OK, row_one()),
        MockResponse::json(token_status, token_body),
        None,
        None,
    ).await
}

fn profile(oauth: Value) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("claude-api-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    std::fs::write(
        dir.path().join("credentials.json"),
        serde_json::to_vec(&json!({"claudeAiOauth": oauth, "unrelated": {"keep": true}})).unwrap(),
    ).unwrap();
    dir
}

#[test]
fn claude_api_01_parse_windows_and_named_models() {
    assert_eq!(parse_usage(&row_one()), Some(expected_usage()));
}

#[test]
fn claude_api_02_empty_usage_is_none() {
    assert_eq!(parse_usage(&json!({})), None);
}

#[test]
fn claude_api_03_spend_converts_cents() {
    let usage = parse_usage(&json!({
        "five_hour": {"utilization": 1},
        "extra_usage": {"is_enabled": true, "used_credits": 1234,
            "monthly_limit": 5000, "utilization": 24.68, "currency": "USD"}
    })).unwrap();
    assert_eq!(usage.five_hour, Some(Window { pct: 1.0, resets_at: None }));
    assert_eq!(usage.spend, Some(Spend {
        used: 12.34, limit: 50.0, pct: 24.68, currency: "USD".into(),
    }));
}

#[test]
fn claude_api_04_null_spend_limit_preserves_window() {
    let usage = parse_usage(&json!({
        "five_hour": {"utilization": 1},
        "extra_usage": {"is_enabled": true, "used_credits": 1234,
            "monthly_limit": null, "utilization": 24.68, "currency": "USD"}
    })).unwrap();
    assert_eq!(usage.spend, None);
    assert_eq!(usage.five_hour, Some(Window { pct: 1.0, resets_at: None }));
}

#[tokio::test]
async fn claude_api_05_fetch_usage_headers_and_body() {
    let server = server(StatusCode::OK, rotation()).await;
    assert_eq!(fetch_usage(&client(), &server.endpoints, "tok").await, Ok(expected_usage()));
    let calls = server.calls();
    assert_eq!(calls.usage_headers.len(), 1);
    assert_eq!(calls.usage_headers[0]["authorization"], "Bearer tok");
    assert_eq!(calls.usage_headers[0]["anthropic-beta"], "oauth-2025-04-20");
    // Anthropic tells only Claude Code whether this week's session reset was
    // used, so the usage request identifies itself the way Claude Code does.
    let agent = calls.usage_headers[0]["user-agent"].to_str().unwrap();
    assert!(agent.starts_with("claude-cli/") && agent.ends_with(" (external, cli)"), "{agent}");
    assert_eq!(calls.usage_headers[0]["x-app"], "cli");
}

#[tokio::test]
async fn claude_api_06_rate_limited_retry_after() {
    let server = ClaudeServer::start(
        MockResponse::json(StatusCode::TOO_MANY_REQUESTS, json!({})),
        MockResponse::json(StatusCode::OK, rotation()), Some("30"), None,
    ).await;
    assert_eq!(fetch_usage(&client(), &server.endpoints, "tok").await,
        Err(UsageError::RateLimited { retry_after: Some(Duration::from_secs(30)) }));
}

#[tokio::test]
async fn claude_api_07_unauthorized() {
    let server = ClaudeServer::start(
        MockResponse::json(StatusCode::UNAUTHORIZED, json!({})),
        MockResponse::json(StatusCode::OK, rotation()), None, None,
    ).await;
    assert_eq!(fetch_usage(&client(), &server.endpoints, "tok").await,
        Err(UsageError::Unauthorized));
}

#[tokio::test]
async fn claude_api_08_refresh_rotates_and_preserves_fields() {
    let server = server(StatusCode::OK, rotation()).await;
    let before = now_ms();
    let outcome = refresh_oauth(&client(), &server.endpoints, &oauth(1)).await;
    let RefreshOutcome::Refreshed(updated) = outcome else {
        panic!("expected Refreshed, got {outcome:?}");
    };
    assert_eq!(updated["accessToken"], "new");
    assert_eq!(updated["refreshToken"], "rt2");
    let expires_at = updated["expiresAt"].as_i64().unwrap();
    assert!((before + 3_600_000 - 5_000..=now_ms() + 3_600_000 + 5_000).contains(&expires_at));
    assert_eq!(updated["scopes"], json!(["user:inference", "user:profile"]));
    assert_eq!(updated["subscriptionType"], "max");
    assert_eq!(server.calls().token_bodies, vec![json!({
        "grant_type": "refresh_token", "refresh_token": "rt1",
        "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
    })]);
}

#[tokio::test]
async fn claude_api_09_invalid_grant_is_dead() {
    let server = server(StatusCode::BAD_REQUEST, json!({"error": "invalid_grant"})).await;
    assert_eq!(refresh_oauth(&client(), &server.endpoints, &oauth(1)).await, RefreshOutcome::Dead);
}

#[tokio::test]
async fn claude_api_10_invalid_client_is_rejected() {
    let server = server(StatusCode::BAD_REQUEST, json!({"error": "invalid_client"})).await;
    assert_eq!(refresh_oauth(&client(), &server.endpoints, &oauth(1)).await, RefreshOutcome::ClientRejected);
}

#[tokio::test]
async fn claude_api_11_server_error_is_transient() {
    let server = server(StatusCode::INTERNAL_SERVER_ERROR, json!({})).await;
    let outcome = refresh_oauth(&client(), &server.endpoints, &oauth(1)).await;
    assert!(matches!(outcome, RefreshOutcome::Transient(_)), "got {outcome:?}");
}

#[tokio::test]
async fn claude_api_12_missing_refresh_token_skips_http() {
    let server = server(StatusCode::OK, rotation()).await;
    assert_eq!(refresh_oauth(&client(), &server.endpoints,
        &json!({"accessToken": "old", "expiresAt": 1})).await, RefreshOutcome::NoRefreshToken);
    assert!(server.calls().token_bodies.is_empty());
    assert!(server.calls().usage_headers.is_empty());
}

#[tokio::test]
async fn claude_api_13_expired_active_token_never_refreshes() {
    let dir = profile(oauth(now_ms() + 3_600_000));
    let live = oauth(now_ms() - 60_000);
    let server = server(StatusCode::OK, rotation()).await;
    assert_eq!(usage_for_profile(&client(), &server.endpoints, dir.path(), true, Some(&live)).await,
        Err(UsageError::TokenExpired));
    assert!(server.calls().token_bodies.is_empty());
    assert!(server.calls().usage_headers.is_empty());
}

#[tokio::test]
async fn claude_api_14_inactive_rotation_persisted_before_usage() {
    let dir = profile(oauth(now_ms() - 60_000));
    let path = dir.path().join("credentials.json");
    let server = ClaudeServer::start(
        MockResponse::json(StatusCode::OK, row_one()),
        MockResponse::json(StatusCode::OK, rotation()), None, Some(path.clone()),
    ).await;
    assert_eq!(usage_for_profile(&client(), &server.endpoints, dir.path(), false, None).await,
        Ok(expected_usage()));
    let calls = server.calls();
    assert_eq!(calls.token_bodies.len(), 1);
    assert_eq!(calls.usage_headers.len(), 1);
    assert_eq!(calls.usage_headers[0]["authorization"], "Bearer new");
    let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["claudeAiOauth"]["refreshToken"], "rt2");
    assert_eq!(saved["claudeAiOauth"]["accessToken"], "new");
    assert_eq!(saved["claudeAiOauth"]["subscriptionType"], "max");
    assert_eq!(saved["unrelated"], json!({"keep": true}));
    assert_eq!(calls.credentials_at_usage, vec![saved]);
}

#[tokio::test]
async fn claude_api_15_unexpired_inactive_uses_stored_token() {
    let original = oauth(now_ms() + 3_600_000);
    let dir = profile(original.clone());
    let server = server(StatusCode::OK, rotation()).await;
    assert_eq!(usage_for_profile(&client(), &server.endpoints, dir.path(), false,
        Some(&json!({"accessToken": "wrong-live-token"}))).await, Ok(expected_usage()));
    let calls = server.calls();
    assert!(calls.token_bodies.is_empty());
    assert_eq!(calls.usage_headers.len(), 1);
    assert_eq!(calls.usage_headers[0]["authorization"], "Bearer old");
    let saved: Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("credentials.json")).unwrap()).unwrap();
    assert_eq!(saved["claudeAiOauth"], original);
}
