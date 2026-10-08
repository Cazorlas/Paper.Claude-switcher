use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};

#[derive(Default)]
struct Calls {
    usage_tokens: Vec<String>,
    token_bodies: Vec<Value>,
}

#[derive(Clone)]
struct MockState {
    a_usage: f64,
    calls: Arc<Mutex<Calls>>,
}

struct Mock {
    base: String,
    calls: Arc<Mutex<Calls>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Mock {
    async fn start(a_usage: f64) -> Self {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let app = Router::new()
            .route("/api/oauth/usage", get(usage_handler))
            .route("/v1/oauth/token", post(token_handler))
            .route("/legacy", get(legacy_handler).post(legacy_handler))
            .with_state(MockState { a_usage, calls: calls.clone() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async { let _ = stopped.await; })
                .await
                .unwrap();
        });
        Self { base, calls, shutdown: Some(shutdown) }
    }

    fn token_calls(&self) -> usize {
        self.calls.lock().unwrap().token_bodies.len()
    }

    fn usage_calls(&self, token: &str) -> usize {
        self.calls.lock().unwrap().usage_tokens.iter()
            .filter(|seen| seen.as_str() == token).count()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn legacy_handler() -> StatusCode {
    StatusCode::UNAUTHORIZED
}

async fn usage_handler(State(state): State<MockState>, headers: HeaderMap) -> Response {
    let token = headers.get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    state.calls.lock().unwrap().usage_tokens.push(token.to_owned());
    let (five, seven) = match token {
        "tokA" => (state.a_usage, 20.0),
        "tokB" => (10.0, 5.0),
        "tokB2" => (11.0, 6.0),
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let reset = |hours| (chrono::Utc::now() + chrono::Duration::hours(hours)).to_rfc3339();
    axum::Json(json!({
        "five_hour": {"utilization": five, "resets_at": reset(5)},
        "seven_day": {"utilization": seven, "resets_at": reset(168)}
    })).into_response()
}

async fn token_handler(
    State(state): State<MockState>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    state.calls.lock().unwrap().token_bodies.push(body.clone());
    if body["refresh_token"] != "rtB" || body["grant_type"] != "refresh_token" {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "invalid_grant"})))
            .into_response();
    }
    axum::Json(json!({
        "access_token": "tokB2", "expires_in": 3600, "refresh_token": "rtB2"
    })).into_response()
}

fn write_json(path: impl AsRef<Path>, value: &Value) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn oauth(token: &str, refresh: &str, expired: bool) -> Value {
    let expires_at = chrono::Utc::now().timestamp_millis()
        + if expired { -3_600_000 } else { 3_600_000 };
    json!({
        "accessToken": token, "refreshToken": refresh,
        "expiresAt": expires_at, "subscriptionType": "max"
    })
}

fn account(uuid: &str, email: &str) -> Value {
    json!({"accountUuid": uuid, "emailAddress": email})
}

struct Fixture {
    root: tempfile::TempDir,
    mock: Mock,
    a: Value,
    b: Value,
}

impl Fixture {
    async fn new(a_usage: f64) -> Self {
        // Keep every fixture and child-process home inside this worktree.
        let root = tempfile::Builder::new().prefix("claude-cli-")
            .tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target")).unwrap();
        for dir in ["claude", "app", "home", "codex", "empty-path"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        Self {
            root, mock: Mock::start(a_usage).await,
            a: oauth("tokA", "rtA", false),
            b: oauth("tokB", "rtB", false),
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn live(&self, oauth: &Value, account: &Value) {
        write_json(self.path("claude/.credentials.json"), &json!({
            "claudeAiOauth": oauth, "mcpOAuth": {"m": 1}
        }));
        write_json(self.path("claude/.claude.json"), &json!({
            "oauthAccount": account, "projects": {"p": 1}
        }));
    }

    fn live_a(&self) {
        self.live(&self.a, &account("U1", "a@x.com"));
    }

    fn profile(&self, alias: &str, oauth: &Value, account: &Value) {
        let dir = self.path("app/profiles").join(alias);
        write_json(dir.join("credentials.json"), &json!({"claudeAiOauth": oauth}));
        write_json(dir.join("account.json"), account);
    }

    fn two_profiles(&self) {
        self.live_a();
        self.profile("personal", &self.a, &account("U1", "a@x.com"));
        self.profile("work", &self.b, &account("U2", "b@x.com"));
        fs::write(self.path("app/current"), "personal").unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_paper-claude-switch"));
        cmd.args(args).current_dir(self.root.path())
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .env("CLAUDE_CONFIG_DIR", self.path("claude"))
            .env("PAPER_CLAUDE_SWITCH_HOME", self.path("app"))
            .env("HOME", self.path("home"))
            .env("USERPROFILE", self.path("home"))
            .env("CODEX_HOME", self.path("codex"))
            .env("CS_CLAUDE_API_BASE", &self.mock.base)
            .env("CS_CLAUDE_TOKEN_URL", format!("{}/v1/oauth/token", self.mock.base))
            .env("CS_TOKEN_URL", format!("{}/legacy", self.mock.base))
            .env("CS_USAGE_URL", format!("{}/legacy", self.mock.base))
            .env("CS_GITHUB_API_URL", &self.mock.base)
            .env("CS_UPDATE_TTL_SECS", "999999999")
            .env("NO_COLOR", "1");
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy",
            "all_proxy", "CS_PROXY", "RUST_LOG"] {
            cmd.env_remove(key);
        }
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        output_with_timeout(&mut self.command(args))
    }
}

fn output_with_timeout(cmd: &mut Command) -> Output {
    let mut child = cmd.spawn().unwrap();
    let read_pipe = |mut pipe: Box<dyn Read + Send>| thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let stdout = read_pipe(Box::new(child.stdout.take().unwrap()));
    let stderr = read_pipe(Box::new(child.stderr.take().unwrap()));
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Output { status, stdout: stdout.join().unwrap(), stderr: stderr.join().unwrap() };
        }
        if started.elapsed() >= Duration::from_secs(30) {
            // This is exclusively the command spawned by this test.
            child.kill().unwrap();
            child.wait().unwrap();
            let stdout = stdout.join().unwrap();
            let stderr = stderr.join().unwrap();
            panic!("command timed out: {cmd:?}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!(
        "invalid JSON: {error}; status: {}; stdout: {}; stderr: {}",
        output.status, String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)
    ))
}

fn success(output: &Output) {
    assert!(output.status.success(), "status: {}; stdout: {}; stderr: {}",
        output.status, String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_saves_current_claude_account() {
    let f = Fixture::new(50.0).await;
    f.live_a();
    let output = f.run(&["--json", "login", "personal"]);
    success(&output);
    let value = report(&output);
    assert_eq!(value["ok"], true);
    assert_eq!(value["alias"], "personal");
    assert_eq!(read_json(f.path("app/profiles/personal/credentials.json")), json!({"claudeAiOauth": f.a}));
    assert_eq!(read_json(f.path("app/profiles/personal/account.json")), account("U1", "a@x.com"));
    assert_eq!(fs::read_to_string(f.path("app/current")).unwrap(), "personal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_without_live_credentials_explains_claude_login() {
    let f = Fixture::new(50.0).await;
    let output = f.run(&["--json", "login"]);
    assert!(!output.status.success());
    let value = report(&output);
    assert_eq!(value["ok"], false);
    assert!(value["error"].as_str().unwrap().contains("/login"), "{value}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_same_account_updates_existing_alias() {
    let f = Fixture::new(50.0).await;
    f.live_a();
    success(&f.run(&["--json", "login", "personal"]));
    let output = f.run(&["--json", "login", "work"]);
    success(&output);
    let value = report(&output);
    assert_eq!(value["ok"], true);
    assert_eq!(value["alias"], "personal");
    assert!(!f.path("app/profiles/work").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn use_position_swaps_only_claude_account_fields() {
    let f = Fixture::new(50.0).await;
    f.two_profiles();
    let output = f.run(&["--json", "use", "2"]);
    success(&output);
    assert_eq!(read_json(f.path("claude/.credentials.json")), json!({
        "claudeAiOauth": f.b, "mcpOAuth": {"m": 1}
    }));
    assert_eq!(read_json(f.path("claude/.claude.json")), json!({
        "oauthAccount": account("U2", "b@x.com"), "projects": {"p": 1}
    }));
    assert_eq!(fs::read_to_string(f.path("app/current")).unwrap(), "work");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn use_rejects_unsaved_live_account_without_changing_files() {
    let f = Fixture::new(50.0).await;
    f.two_profiles();
    f.live(&oauth("tokC", "rtC", false), &account("U3", "c@x.com"));
    let credentials = fs::read(f.path("claude/.credentials.json")).unwrap();
    let config = fs::read(f.path("claude/.claude.json")).unwrap();
    let output = f.run(&["use", "work"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("login"),
        "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read(f.path("claude/.credentials.json")).unwrap(), credentials);
    assert_eq!(fs::read(f.path("claude/.claude.json")).unwrap(), config);
}

fn profiles(output: &Output) -> Vec<Value> {
    success(output);
    report(output)["profiles"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_reports_alphabetical_profiles_and_claude_usage() {
    let f = Fixture::new(50.0).await;
    f.two_profiles();
    let rows = profiles(&f.run(&["--json", "list"]));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["alias"], "personal");
    assert_eq!(rows[1]["alias"], "work");
    assert_eq!(rows[0]["is_current"], true);
    assert_eq!(rows[0]["usage"]["primary"]["used_percent"], 50.0);
    assert_eq!(rows[0]["usage"]["secondary"]["used_percent"], 20.0);
    assert_eq!(rows[0]["account"]["email"], "a@x.com");
    assert_eq!(rows[0]["account"]["plan"], "max");
    assert_eq!(rows[1]["is_current"], false);
    assert_eq!(rows[1]["usage"]["primary"]["used_percent"], 10.0);
    assert_eq!(rows[1]["usage"]["secondary"]["used_percent"], 5.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_identifies_live_uuid_repairs_marker_and_never_refreshes_active() {
    let f = Fixture::new(50.0).await;
    f.two_profiles();
    f.profile("personal", &oauth("expiredStoredA", "rtA", true), &account("U1", "a@x.com"));
    fs::write(f.path("app/current"), "work").unwrap();
    let rows = profiles(&f.run(&["--json", "list"]));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["alias"], "personal");
    assert_eq!(rows[0]["is_current"], true);
    assert_eq!(rows[1]["is_current"], false);
    assert_eq!(rows[0]["usage"]["primary"]["used_percent"], 50.0);
    assert_eq!(f.mock.usage_calls("tokA"), 1);
    assert_eq!(f.mock.usage_calls("expiredStoredA"), 0);
    assert_eq!(f.mock.token_calls(), 0);
    assert_eq!(fs::read_to_string(f.path("app/current")).unwrap(), "personal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_refreshes_expired_inactive_account_and_persists_rotation() {
    let f = Fixture::new(50.0).await;
    f.two_profiles();
    f.profile("work", &oauth("tokB", "rtB", true), &account("U2", "b@x.com"));
    let rows = profiles(&f.run(&["--json", "list"]));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["alias"], "work");
    assert_eq!(rows[1]["usage"]["primary"]["used_percent"], 11.0);
    assert_eq!(f.mock.token_calls(), 1);
    assert_eq!(f.mock.usage_calls("tokB2"), 1);
    let stored = read_json(f.path("app/profiles/work/credentials.json"));
    assert_eq!(stored["claudeAiOauth"]["refreshToken"], "rtB2");
    assert_eq!(stored["claudeAiOauth"]["accessToken"], "tokB2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn use_without_alias_selects_best_claude_account() {
    let f = Fixture::new(95.0).await;
    f.two_profiles();
    let output = f.run(&["--json", "use"]);
    success(&output);
    assert_eq!(report(&output)["switched_to"], "work");
    assert_eq!(read_json(f.path("claude/.claude.json"))["oauthAccount"]["accountUuid"], "U2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_reports_missing_claude_and_resolved_credentials_path() {
    let f = Fixture::new(50.0).await;
    f.live_a();
    let mut cmd = f.command(&["--json", "doctor"]);
    cmd.env("PATH", f.path("empty-path"));
    let output = output_with_timeout(&mut cmd);
    assert_eq!(output.status.code(), Some(1));
    let value = report(&output);
    assert_eq!(value["credentials_path"], f.path("claude").join(".credentials.json").to_string_lossy().as_ref());
    assert_eq!(value["live_login"], true);
    assert_eq!(value.get("claude_version"), Some(&Value::Null));
}
