// Using a usage-limit reset: `POST /api/organizations/{org}/reset_rate_limits`.
// Every test talks to a local mock; the real endpoint spends a real reset.

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    routing::any,
};
use claude_switch::claude_api::{
    ClaimError, ClaimResult, Endpoints, ResetClaim, UsageError, claim_reset_grant,
};

const ORG: &str = "0f4077c3-79c6-44ff-a1f0-f89492f28d52";

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    authorization: Option<String>,
    beta: Option<String>,
    body: Vec<u8>,
}

#[derive(Clone)]
struct Mock {
    status: u16,
    body: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

async fn handle(
    State(mock): State<Mock>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    mock.seen.lock().unwrap().push(Seen {
        method: method.to_string(),
        path: uri.path().to_owned(),
        authorization: header("authorization"),
        beta: header("anthropic-beta"),
        body: body.to_vec(),
    });
    (StatusCode::from_u16(mock.status).unwrap(), mock.body.clone())
}

/// Serves `status` and `body` for every request; returns the endpoints and the
/// requests the mock has seen.
async fn serve(status: u16, body: &str) -> (Endpoints, Arc<Mutex<Vec<Seen>>>) {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let mock = Mock { status, body: body.to_owned(), seen: seen.clone() };
    let app = Router::new().fallback(any(handle)).with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (Endpoints { api_base: base.clone(), token_url: format!("{base}/token") }, seen)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn claim(status: u16, body: &str) -> Result<ResetClaim, ClaimError> {
    let (ep, _seen) = serve(status, body).await;
    claim_reset_grant(&client(), &ep, "tok", ORG, "g2", "req_1").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_posts_the_grant_and_reads_the_reply() {
    let (ep, seen) = serve(200, r#"{"result":"reset","resets_left":1,"cleared":["seven_day"]}"#).await;

    let claim = claim_reset_grant(&client(), &ep, "tok", ORG, "g2", "req_1").await;

    assert_eq!(
        claim,
        Ok(ResetClaim {
            result: ClaimResult::Reset,
            resets_left: Some(1),
            cleared: vec!["seven_day".to_string()],
            cooldown_until: None,
        })
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, format!("/api/organizations/{ORG}/reset_rate_limits"));
    assert_eq!(seen[0].authorization.as_deref(), Some("Bearer tok"));
    assert_eq!(seen[0].beta.as_deref(), Some("oauth-2025-04-20"));
    let body: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"program": "cedar_ember", "grant_id": "g2", "request_id": "req_1"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_server_result_maps_to_a_claim_result() {
    for (body, expected) in [
        (r#"{"result":"already_used"}"#, ClaimResult::AlreadyUsed),
        (r#"{"result":"not_limited"}"#, ClaimResult::NotLimited),
        (r#"{"result":"ineligible"}"#, ClaimResult::Ineligible),
        (r#"{"result":"unavailable"}"#, ClaimResult::Unavailable),
        (r#"{"result":"something_new"}"#, ClaimResult::Unavailable),
    ] {
        let claim = claim(200, body).await.unwrap_or_else(|e| panic!("{body}: {e:?}"));
        assert_eq!(claim.result, expected, "{body}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cooldown_carries_its_end_time() {
    let claim = claim(
        200,
        r#"{"result":"cooldown","cooldown_until":"2026-10-10T00:00:00Z","resets_left":null}"#,
    )
    .await
    .unwrap();
    assert_eq!(claim.result, ClaimResult::Cooldown);
    assert_eq!(claim.cooldown_until.as_deref(), Some("2026-10-10T00:00:00Z"));
    assert_eq!(claim.resets_left, None);
    assert!(claim.cleared.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_and_auth_errors_mean_no_reset_was_used() {
    assert!(matches!(
        claim(429, "{}").await,
        Err(ClaimError::Rejected(UsageError::RateLimited { .. }))
    ));
    assert_eq!(claim(401, "{}").await, Err(ClaimError::Rejected(UsageError::Unauthorized)));
    assert_eq!(claim(403, "{}").await, Err(ClaimError::Rejected(UsageError::Unauthorized)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_error_or_unreadable_reply_leaves_it_unknown() {
    assert!(matches!(claim(500, "oops").await, Err(ClaimError::Unknown(_))));
    assert!(matches!(claim(200, "not json").await, Err(ClaimError::Unknown(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ids_that_claude_code_would_refuse_are_never_sent() {
    let long_request = "r".repeat(65);
    let cases: [(&str, &str, &str); 6] = [
        (ORG, "G 1!", "req_1"),
        (ORG, "", "req_1"),
        (ORG, "g2", ""),
        (ORG, "g2", long_request.as_str()),
        ("../x", "g2", "req_1"),
        ("", "g2", "req_1"),
    ];
    for (org, grant, request) in cases {
        let (ep, seen) = serve(200, r#"{"result":"reset"}"#).await;
        let outcome = claim_reset_grant(&client(), &ep, "tok", org, grant, request).await;
        assert!(
            matches!(outcome, Err(ClaimError::Rejected(_))),
            "org {org:?} grant {grant:?} request {request:?}: {outcome:?}"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            0,
            "org {org:?} grant {grant:?} request {request:?} must not reach the server"
        );
    }
}
