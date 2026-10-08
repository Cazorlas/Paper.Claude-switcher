// A 429 from the usage endpoint keeps the last good reading and pauses calls
// for that alias until Retry-After has passed. Checked through the library API
// with the process environment redirected to temp folders under target/.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Router,
    extract::State,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use claude_switch::usage::UsageInfo;
use serde_json::{Value, json};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// One programmed usage reply.
#[derive(Clone, Copy)]
enum Reply {
    Ok { five: f64 },
    Limited { retry_after: u64 },
}

#[derive(Default)]
struct MockState {
    replies: VecDeque<Reply>,
    usage_calls: usize,
}

struct Mock {
    base: String,
    state: Arc<Mutex<MockState>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Mock {
    async fn start(replies: &[Reply]) -> Self {
        let state = Arc::new(Mutex::new(MockState {
            replies: replies.iter().copied().collect(),
            usage_calls: 0,
        }));
        let app = Router::new()
            .route("/api/oauth/usage", get(usage_handler))
            .route("/v1/oauth/token", post(|| async { StatusCode::UNAUTHORIZED }))
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

    fn usage_calls(&self) -> usize {
        self.state.lock().unwrap().usage_calls
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn usage_handler(State(state): State<Arc<Mutex<MockState>>>) -> Response {
    let mut state = state.lock().unwrap();
    state.usage_calls += 1;
    match state.replies.pop_front() {
        Some(Reply::Ok { five }) => axum::Json(json!({
            "five_hour": {"utilization": five, "resets_at": "2099-01-01T00:00:00Z"},
            "seven_day": {"utilization": 1.0, "resets_at": "2099-01-05T00:00:00Z"}
        }))
        .into_response(),
        Some(Reply::Limited { retry_after }) => {
            let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
            response.headers_mut().insert(
                "retry-after",
                HeaderValue::from_str(&retry_after.to_string()).unwrap(),
            );
            response
        }
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
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
    /// Point every path and endpoint the library reads at `root` and `mock`,
    /// with an inactive profile `work` holding an unexpired token.
    fn new(mock: &Mock) -> Self {
        let root = tempfile::Builder::new()
            .prefix("claude-ratelimit-")
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
            "expiresAt": chrono::Utc::now().timestamp_millis() + 3_600_000,
            "subscriptionType": "max"
        }}));
        write_json(dir.join("account.json"), &json!({
            "accountUuid": "U2", "emailAddress": "work@x.com"
        }));
        env
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }
}

async fn fetch_forced() -> Result<UsageInfo, claude_switch::usage::UsageError> {
    let profile = claude_switch::claude_usage::read_profile("work").unwrap();
    claude_switch::claude_usage::fetch(&profile, None, true).await
}

fn primary(usage: &UsageInfo) -> Option<f64> {
    usage.primary.as_ref().and_then(|window| window.used_percent)
}

/// R1: a 429 after a good reading answers with that reading, unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_returns_last_good_usage_with_its_old_time() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start(&[Reply::Ok { five: 50.0 }, Reply::Limited { retry_after: 120 }]).await;
    let _env = Env::new(&mock);

    let first = fetch_forced().await.unwrap_or_else(|e| panic!("first fetch failed: {e:?}"));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let second = fetch_forced().await.unwrap_or_else(|e| panic!("second fetch failed: {e:?}"));

    assert_eq!(primary(&first), Some(50.0));
    assert_eq!(primary(&second), Some(50.0));
    assert_eq!(second.fetched_at, first.fetched_at, "the old measurement time stays");
    assert_eq!(mock.usage_calls(), 2);
}

/// R2: while the pause is running, no request goes out at all, forced or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_pause_blocks_further_requests() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start(&[Reply::Ok { five: 50.0 }, Reply::Limited { retry_after: 120 }]).await;
    let _env = Env::new(&mock);

    fetch_forced().await.unwrap();
    fetch_forced().await.unwrap();
    let third = fetch_forced().await.unwrap_or_else(|e| panic!("third fetch failed: {e:?}"));

    assert_eq!(primary(&third), Some(50.0));
    assert_eq!(mock.usage_calls(), 2, "the paused alias must not be called again");
}

/// R3: with no good reading to fall back on the row is an error that names the
/// rate limit, and the pause still stops the second request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_without_last_good_usage_is_an_error() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start(&[Reply::Limited { retry_after: 120 }]).await;
    let _env = Env::new(&mock);

    let first = fetch_forced().await;
    let second = fetch_forced().await;

    for result in [first, second] {
        let error = result.expect_err("no last good usage: the row must be an error");
        assert!(error.summary.contains("rate limited"), "summary: {}", error.summary);
    }
    assert_eq!(mock.usage_calls(), 1);
}

/// R4: once Retry-After has passed the endpoint is called again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_pause_ends_after_retry_after() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start(&[Reply::Limited { retry_after: 1 }, Reply::Ok { five: 10.0 }]).await;
    let _env = Env::new(&mock);

    let first = fetch_forced().await;
    assert!(first.is_err(), "first reply is a 429 with nothing to fall back on");
    let during = fetch_forced().await;
    assert!(during.is_err(), "still paused: {during:?}");
    assert_eq!(mock.usage_calls(), 1, "no request while the pause runs");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let second = fetch_forced().await.unwrap_or_else(|e| panic!("second fetch failed: {e:?}"));

    assert_eq!(primary(&second), Some(10.0));
    assert_eq!(mock.usage_calls(), 2);
}
