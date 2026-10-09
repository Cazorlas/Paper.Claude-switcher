// Using a usage-limit reset for a saved profile: `claude_usage::claim_reset`
// reads the profile, gets a token the way `fetch` does, picks the usable grant
// from fresh usage and posts the claim. Every test talks to a local mock; the
// real endpoint spends a real reset. Process environment is redirected to temp
// folders under target/.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use claude_switch::claude_api::{ClaimError, ClaimResult};
use serde_json::{Value, json};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone)]
struct Claim {
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

#[derive(Default)]
struct MockState {
    usage: Value,
    claims: Vec<Claim>,
    token_calls: usize,
}

struct Mock {
    base: String,
    state: Arc<Mutex<MockState>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Mock {
    async fn start(usage: Value) -> Self {
        let state = Arc::new(Mutex::new(MockState { usage, ..Default::default() }));
        let app = Router::new()
            .route("/api/oauth/usage", get(usage_handler))
            .route("/api/organizations/{org}/reset_rate_limits", post(claim_handler))
            .route("/v1/oauth/token", post(token_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self { base, state, shutdown: Some(shutdown) }
    }

    fn claims(&self) -> Vec<Claim> {
        self.state.lock().unwrap().claims.clone()
    }

    fn token_calls(&self) -> usize {
        self.state.lock().unwrap().token_calls
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn usage_handler(State(state): State<Arc<Mutex<MockState>>>) -> axum::Json<Value> {
    axum::Json(state.lock().unwrap().usage.clone())
}

async fn claim_handler(
    State(state): State<Arc<Mutex<MockState>>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::Json<Value> {
    state.lock().unwrap().claims.push(Claim {
        path: uri.path().to_owned(),
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body: body.to_vec(),
    });
    axum::Json(json!({"result": "reset", "resets_left": 0}))
}

async fn token_handler(State(state): State<Arc<Mutex<MockState>>>) -> StatusCode {
    state.lock().unwrap().token_calls += 1;
    StatusCode::UNAUTHORIZED
}

fn write_json(path: impl AsRef<Path>, value: &Value) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

struct Env {
    root: tempfile::TempDir,
}

impl Env {
    /// Point every path and endpoint the library reads at `root` and `mock`.
    /// Profile `work` (account U2) holds a token that expires after `expires_in_ms`
    /// and, when `organization` is set, its account.json names that organization.
    fn new(mock: &Mock, expires_in_ms: i64, organization: Option<&str>) -> Self {
        let root = tempfile::Builder::new()
            .prefix("claude-reset-use-")
            .tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
            .unwrap();
        for dir in ["claude", "app", "home"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        // SAFETY: tests in this binary hold ENV_LOCK while the variables are in use.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", root.path().join("claude"));
            std::env::set_var("PAPER_CLAUDE_SWITCH_HOME", root.path().join("app"));
            std::env::set_var("HOME", root.path().join("home"));
            std::env::set_var("USERPROFILE", root.path().join("home"));
            std::env::set_var("CS_CLAUDE_API_BASE", &mock.base);
            std::env::set_var("CS_CLAUDE_TOKEN_URL", format!("{}/v1/oauth/token", mock.base));
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy",
                "all_proxy", "CS_PROXY"] {
                std::env::remove_var(key);
            }
        }
        let env = Self { root };
        let dir = env.path("app/profiles/work");
        write_json(dir.join("credentials.json"), &json!({"claudeAiOauth": {
            "accessToken": "tokWork",
            "refreshToken": "rtWork",
            "expiresAt": chrono::Utc::now().timestamp_millis() + expires_in_ms,
            "subscriptionType": "max"
        }}));
        let mut account = json!({"accountUuid": "U2", "emailAddress": "work@x.com"});
        if let Some(organization) = organization {
            account["organizationUuid"] = json!(organization);
        }
        write_json(dir.join("account.json"), &account);
        env
    }

    /// Make `work` the live Claude login, with its own live token.
    fn log_in_work(&self, expires_in_ms: i64) {
        write_json(self.path("claude/.credentials.json"), &json!({"claudeAiOauth": {
            "accessToken": "tokLive",
            "refreshToken": "rtLive",
            "expiresAt": chrono::Utc::now().timestamp_millis() + expires_in_ms,
            "subscriptionType": "max"
        }}));
        write_json(self.path("claude/.claude.json"), &json!({"oauthAccount": {
            "accountUuid": "U2", "emailAddress": "work@x.com", "organizationUuid": "org-1"
        }}));
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }
}

fn usage_with_next(next: Value) -> Value {
    json!({
        "five_hour": {"utilization": 100.0, "resets_at": "2099-01-01T00:00:00Z"},
        "seven_day": {"utilization": 40.0, "resets_at": "2099-01-05T00:00:00Z"},
        "cedar_ember": {
            "eligible": true,
            "next_grant_id": next,
            "grants": [{"id": "g2", "label": "b", "resets_total": 1, "resets_left": 1}]
        }
    })
}

/// U1: an inactive profile claims its next grant with its own saved token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inactive_profile_claims_the_next_grant_with_its_saved_token() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start(usage_with_next(json!("g2"))).await;
    let _env = Env::new(&mock, 3_600_000, Some("org-1"));

    let claim = claude_switch::claude_usage::claim_reset("work", "req_7")
        .await
        .unwrap_or_else(|e| panic!("claim failed: {e:?}"));

    assert_eq!(claim.result, ClaimResult::Reset);
    assert_eq!(claim.resets_left, Some(0));
    let claims = mock.claims();
    assert_eq!(claims.len(), 1, "exactly one POST: {claims:?}");
    assert_eq!(claims[0].path, "/api/organizations/org-1/reset_rate_limits");
    assert_eq!(claims[0].authorization.as_deref(), Some("Bearer tokWork"));
    let body: Value = serde_json::from_slice(&claims[0].body).unwrap();
    assert_eq!(
        body,
        json!({"program": "cedar_ember", "grant_id": "g2", "request_id": "req_7"})
    );
}

/// U2: no usable grant in fresh usage: refused before anything is posted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_usable_grant_is_rejected_and_nothing_is_posted() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start(usage_with_next(Value::Null)).await;
    let _env = Env::new(&mock, 3_600_000, Some("org-1"));

    let result = claude_switch::claude_usage::claim_reset("work", "req_7").await;

    assert!(matches!(result, Err(ClaimError::Rejected(_))), "result: {result:?}");
    assert!(mock.claims().is_empty(), "nothing may be posted");
}

/// U3: no organization uuid in account.json: refused before anything is posted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_organization_is_rejected_and_nothing_is_posted() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start(usage_with_next(json!("g2"))).await;
    let _env = Env::new(&mock, 3_600_000, None);

    let result = claude_switch::claude_usage::claim_reset("work", "req_7").await;

    assert!(matches!(result, Err(ClaimError::Rejected(_))), "result: {result:?}");
    assert!(mock.claims().is_empty(), "nothing may be posted");
}

/// U4: the active profile uses the live token and is never refreshed: an
/// expired live token is refused, with no token-endpoint call and no POST.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_profile_with_an_expired_live_token_is_not_refreshed() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start(usage_with_next(json!("g2"))).await;
    let env = Env::new(&mock, 3_600_000, Some("org-1"));
    env.log_in_work(-3_600_000);

    let result = claude_switch::claude_usage::claim_reset("work", "req_7").await;

    assert!(matches!(result, Err(ClaimError::Rejected(_))), "result: {result:?}");
    assert!(mock.claims().is_empty(), "nothing may be posted");
    assert_eq!(mock.token_calls(), 0, "the live token is never refreshed from here");
}
