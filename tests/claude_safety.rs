// Safety rules around token refresh, checked through the library API. The
// process environment is redirected to temp folders before anything runs, so
// the real Claude login is never read or written.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct Calls {
    usage_tokens: Vec<String>,
    token_calls: usize,
}

struct Mock {
    base: String,
    calls: Arc<Mutex<Calls>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Mock {
    async fn start() -> Self {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let app = Router::new()
            .route("/api/oauth/usage", get(usage_handler))
            .route("/v1/oauth/token", post(token_handler))
            .with_state(calls.clone());
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
        Self { base, calls, shutdown: Some(shutdown) }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn usage_handler(State(calls): State<Arc<Mutex<Calls>>>, headers: HeaderMap) -> Response {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_owned();
    calls.lock().unwrap().usage_tokens.push(token.clone());
    let five = match token.as_str() {
        "tokLive" => 42.0,
        "tokRotated" => 7.0,
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    axum::Json(json!({
        "five_hour": {"utilization": five, "resets_at": "2099-01-01T00:00:00Z"},
        "seven_day": {"utilization": 1.0, "resets_at": "2099-01-05T00:00:00Z"}
    }))
    .into_response()
}

async fn token_handler(State(calls): State<Arc<Mutex<Calls>>>) -> Response {
    calls.lock().unwrap().token_calls += 1;
    axum::Json(json!({
        "access_token": "tokRotated", "expires_in": 3600, "refresh_token": "rtRotated"
    }))
    .into_response()
}

fn write_json(path: impl AsRef<Path>, value: &Value) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn oauth(token: &str, refresh: &str, expired: bool) -> Value {
    let offset = if expired { -3_600_000 } else { 3_600_000 };
    json!({
        "accessToken": token,
        "refreshToken": refresh,
        "expiresAt": chrono::Utc::now().timestamp_millis() + offset,
        "subscriptionType": "pro"
    })
}

struct Env {
    root: tempfile::TempDir,
}

impl Env {
    /// Point every path and endpoint the library reads at `root` and `mock`.
    fn new(mock: &Mock) -> Self {
        let root = tempfile::Builder::new()
            .prefix("claude-safety-")
            .tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
            .unwrap();
        // SAFETY: tests in this binary hold ENV_LOCK while the variables are in use.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", root.path().join("claude"));
            std::env::set_var("PAPER_CLAUDE_SWITCH_HOME", root.path().join("app"));
            std::env::set_var("CS_CLAUDE_API_BASE", &mock.base);
            std::env::set_var("CS_CLAUDE_TOKEN_URL", format!("{}/v1/oauth/token", mock.base));
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy",
                "all_proxy", "CS_PROXY"] {
                std::env::remove_var(key);
            }
        }
        Self { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn profile(&self, alias: &str, oauth: Value, uuid: &str) {
        let dir = self.path("app/profiles").join(alias);
        write_json(dir.join("credentials.json"), &json!({"claudeAiOauth": oauth}));
        write_json(dir.join("account.json"), &json!({"accountUuid": uuid, "emailAddress": format!("{alias}@x.com")}));
    }

    fn live(&self, oauth: Value, uuid: &str) {
        write_json(self.path("claude/.credentials.json"), &json!({"claudeAiOauth": oauth}));
        write_json(
            self.path("claude/.claude.json"),
            &json!({"oauthAccount": {"accountUuid": uuid, "emailAddress": "live@x.com"}}),
        );
    }
}

/// A caller decided `work` was inactive, but by the time the refresh lock is
/// held `work` is the live account (another process switched to it). Its stored
/// token is expired; refreshing it would rotate the token Claude Code is using.
/// The fetch must notice, use the live token and never call the token endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inactive_fetch_rechecks_the_live_account_before_refreshing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start().await;
    let env = Env::new(&mock);
    env.profile("personal", oauth("tokPersonal", "rtPersonal", false), "U1");
    env.profile("work", oauth("tokStale", "rtStale", true), "U2");
    env.live(oauth("tokLive", "rtLive", false), "U2");
    let before = fs::read(env.path("app/profiles/work/credentials.json")).unwrap();

    let profile = claude_switch::claude_usage::read_profile("work").unwrap();
    let usage = claude_switch::claude_usage::fetch(&profile, None, true).await;

    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.token_calls, 0, "the live account's token must never be refreshed");
    assert_eq!(calls.usage_tokens, vec!["tokLive".to_owned()]);
    drop(calls);
    let usage = usage.unwrap_or_else(|e| panic!("fetch failed: {e:?}"));
    assert_eq!(usage.primary.unwrap().used_percent, Some(42.0));
    assert_eq!(fs::read(env.path("app/profiles/work/credentials.json")).unwrap(), before);
}

/// When the rotated token of an inactive profile cannot be saved, the fetch
/// fails for that account and never goes on to the usage request with a
/// token that exists nowhere on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotated_token_that_cannot_be_saved_fails_the_account() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start().await;
    let env = Env::new(&mock);
    env.profile("personal", oauth("tokLive", "rtLive", false), "U1");
    env.profile("work", oauth("tokStale", "rtStale", true), "U2");
    env.live(oauth("tokLive", "rtLive", false), "U1");
    let creds = env.path("app/profiles/work/credentials.json");
    #[cfg_attr(not(unix), allow(unused_variables))]
    let dir = env.path("app/profiles/work");
    // Block the atomic replace: a read-only target on Windows, a read-only
    // directory on Unix.
    let mut perms = fs::metadata(&creds).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&creds, perms).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    }

    let profile = claude_switch::claude_usage::read_profile("work").unwrap();
    let result = claude_switch::claude_usage::fetch(&profile, None, true).await;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut perms = fs::metadata(&creds).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(&creds, perms).unwrap();

    assert!(result.is_err(), "fetch must fail when the rotated token was not saved");
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.token_calls, 1);
    assert!(calls.usage_tokens.is_empty(), "usage must not be requested: {:?}", calls.usage_tokens);
}

/// No user-facing help text still talks about Codex or OpenAI.
#[test]
fn help_texts_name_claude_not_codex() {
    let bin = env!("CARGO_BIN_EXE_paper-claude-switch");
    let help = |args: &[&str]| {
        let output = Command::new(bin).args(args).env("NO_COLOR", "1").output().unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let top = help(&["--help"]);
    assert!(top.contains("Claude"), "{top}");
    for sub in ["use", "auto", "list", "rename", "delete", "restore", "login", "self-update",
        "launch", "tui", "open", "doctor"] {
        let text = help(&[sub, "--help"]);
        assert!(!text.is_empty(), "no help for {sub}");
        let lower = text.to_lowercase();
        for word in ["codex", "openai", "chatgpt", "auth.json"] {
            assert!(!lower.contains(word), "`{sub} --help` mentions {word}:\n{text}");
        }
    }
    let lower = top.to_lowercase();
    for word in ["codex", "openai", "chatgpt", "auth.json"] {
        assert!(!lower.contains(word), "`--help` mentions {word}:\n{top}");
    }
}
