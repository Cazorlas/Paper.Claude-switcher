// Account management, auto-switching and launch on Claude profiles. Every
// spawned command points CLAUDE_CONFIG_DIR and PAPER_CLAUDE_SWITCH_HOME at a
// temp folder, so the real Claude login is never read or written.

use std::collections::HashMap;
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

#[derive(Clone)]
struct MockState {
    usage: Arc<HashMap<String, (f64, f64)>>,
    token_calls: Arc<Mutex<usize>>,
}

struct Mock {
    base: String,
    token_calls: Arc<Mutex<usize>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Mock {
    async fn start(usage: &[(&str, f64, f64)]) -> Self {
        let token_calls = Arc::new(Mutex::new(0));
        let state = MockState {
            usage: Arc::new(
                usage
                    .iter()
                    .map(|(token, five, seven)| (token.to_string(), (*five, *seven)))
                    .collect(),
            ),
            token_calls: token_calls.clone(),
        };
        let app = Router::new()
            .route("/api/oauth/usage", get(usage_handler))
            .route("/v1/oauth/token", post(token_handler))
            .route("/legacy", get(legacy_handler).post(legacy_handler))
            .with_state(state);
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
        Self { base, token_calls, shutdown: Some(shutdown) }
    }

    fn token_calls(&self) -> usize {
        *self.token_calls.lock().unwrap()
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
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let Some((five, seven)) = state.usage.get(token).copied() else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let reset = |hours| (chrono::Utc::now() + chrono::Duration::hours(hours)).to_rfc3339();
    axum::Json(json!({
        "five_hour": {"utilization": five, "resets_at": reset(4)},
        "seven_day": {"utilization": seven, "resets_at": reset(100)}
    }))
    .into_response()
}

async fn token_handler(State(state): State<MockState>) -> Response {
    *state.token_calls.lock().unwrap() += 1;
    (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_grant"}))).into_response()
}

fn write_json(path: impl AsRef<Path>, value: &Value) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn oauth(token: &str) -> Value {
    json!({
        "accessToken": token,
        "refreshToken": format!("rt-{token}"),
        "expiresAt": chrono::Utc::now().timestamp_millis() + 3_600_000,
        "subscriptionType": "max"
    })
}

fn account(uuid: &str, email: &str) -> Value {
    json!({"accountUuid": uuid, "emailAddress": email})
}

struct Fixture {
    root: tempfile::TempDir,
    mock: Mock,
}

impl Fixture {
    async fn new(usage: &[(&str, f64, f64)]) -> Self {
        let root = tempfile::Builder::new()
            .prefix("claude-manage-")
            .tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
            .unwrap();
        for dir in ["claude", "app", "home", "codex"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        Self { root, mock: Mock::start(usage).await }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn live(&self, token: &str, uuid: &str, email: &str) {
        write_json(
            self.path("claude/.credentials.json"),
            &json!({"claudeAiOauth": oauth(token), "mcpOAuth": {"m": 1}}),
        );
        write_json(
            self.path("claude/.claude.json"),
            &json!({"oauthAccount": account(uuid, email), "projects": {"p": 1}}),
        );
    }

    fn profile(&self, alias: &str, token: &str, uuid: &str, email: &str) {
        let dir = self.path("app/profiles").join(alias);
        write_json(dir.join("credentials.json"), &json!({"claudeAiOauth": oauth(token)}));
        write_json(dir.join("account.json"), &account(uuid, email));
    }

    /// personal (U1, tokA) is live; work (U2, tokB) is saved and inactive.
    fn two_profiles(&self) {
        self.live("tokA", "U1", "a@x.com");
        self.profile("personal", "tokA", "U1", "a@x.com");
        self.profile("work", "tokB", "U2", "b@x.com");
        fs::write(self.path("app/current"), "personal").unwrap();
    }

    fn live_uuid(&self) -> Value {
        read_json(self.path("claude/.claude.json"))["oauthAccount"]["accountUuid"].clone()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_paper-claude-switch"));
        cmd.args(args)
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
        for key in [
            "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy",
            "CS_PROXY", "RUST_LOG",
        ] {
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
    let read_pipe = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        })
    };
    let stdout = read_pipe(Box::new(child.stdout.take().unwrap()));
    let stderr = read_pipe(Box::new(child.stderr.take().unwrap()));
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Output { status, stdout: stdout.join().unwrap(), stderr: stderr.join().unwrap() };
        }
        if started.elapsed() >= Duration::from_secs(30) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("command timed out: {cmd:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}; stdout: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", describe(output));
}

fn archived_credentials(f: &Fixture) -> Vec<PathBuf> {
    fn visit(dir: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, found);
            } else if path.file_name().is_some_and(|n| n == "credentials.json") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    visit(&f.path("app/deleted-profiles"), &mut found);
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_inactive_claude_profile_archives_it() {
    let f = Fixture::new(&[("tokA", 50.0, 20.0), ("tokB", 10.0, 5.0)]).await;
    f.two_profiles();
    let output = f.run(&["delete", "work", "--yes"]);
    success(&output);
    assert!(!f.path("app/profiles/work").exists());
    assert_eq!(archived_credentials(&f).len(), 1, "{}", describe(&output));
    assert!(f.path("app/profiles/personal/credentials.json").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_refuses_the_live_account_even_when_marker_names_another() {
    let f = Fixture::new(&[("tokA", 50.0, 20.0), ("tokB", 10.0, 5.0)]).await;
    f.two_profiles();
    fs::write(f.path("app/current"), "work").unwrap();
    let output = f.run(&["delete", "personal", "--yes"]);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("active"), "{}", describe(&output));
    assert!(f.path("app/profiles/personal/credentials.json").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_brings_back_a_deleted_claude_profile() {
    let f = Fixture::new(&[("tokA", 50.0, 20.0), ("tokB", 10.0, 5.0)]).await;
    f.two_profiles();
    success(&f.run(&["delete", "work", "--yes"]));
    let output = f.run(&["--json", "restore", "work"]);
    success(&output);
    assert_eq!(
        read_json(f.path("app/profiles/work/credentials.json"))["claudeAiOauth"]["accessToken"],
        "tokB"
    );
    assert_eq!(read_json(f.path("app/profiles/work/account.json"))["accountUuid"], "U2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renamed_claude_profile_can_be_used() {
    let f = Fixture::new(&[("tokA", 50.0, 20.0), ("tokB", 10.0, 5.0)]).await;
    f.two_profiles();
    success(&f.run(&["rename", "work", "job"]));
    assert!(!f.path("app/profiles/work").exists());
    assert!(f.path("app/profiles/job/credentials.json").exists());
    success(&f.run(&["--json", "use", "job"]));
    assert_eq!(f.live_uuid(), "U2");
    assert_eq!(fs::read_to_string(f.path("app/current")).unwrap(), "job");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_json_list_saves_the_live_account_and_keeps_stdout_json() {
    let f = Fixture::new(&[("tokA", 50.0, 20.0)]).await;
    f.live("tokA", "U1", "a@x.com");
    let output = f.run(&["--json", "list"]);
    success(&output);
    let value: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}", describe(&output)));
    assert_eq!(value["profiles"][0]["alias"], "a");
    assert_eq!(value["profiles"][0]["is_current"], true);
    assert_eq!(value["profiles"][0]["usage"]["primary"]["used_percent"], 50.0);
    assert!(f.path("app/profiles/a/credentials.json").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Saved"), "{}", describe(&output));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_once_switches_off_a_hot_account() {
    let f = Fixture::new(&[("tokA", 95.0, 20.0), ("tokB", 10.0, 5.0)]).await;
    f.two_profiles();
    let output = f.run(&["--json", "auto", "--once"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(f.live_uuid(), "U2");
    assert_eq!(fs::read_to_string(f.path("app/current")).unwrap(), "work");
    let creds = read_json(f.path("claude/.credentials.json"));
    assert_eq!(creds["claudeAiOauth"]["accessToken"], "tokB");
    assert_eq!(creds["mcpOAuth"], json!({"m": 1}));
    assert_eq!(f.mock.token_calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_once_stays_below_the_threshold() {
    let f = Fixture::new(&[("tokA", 50.0, 20.0), ("tokB", 10.0, 5.0)]).await;
    f.two_profiles();
    let before = fs::read(f.path("claude/.credentials.json")).unwrap();
    let output = f.run(&["--json", "auto", "--once"]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert_eq!(f.live_uuid(), "U1");
    assert_eq!(fs::read(f.path("claude/.credentials.json")).unwrap(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_once_is_blocked_when_every_account_is_hot() {
    let f = Fixture::new(&[("tokA", 95.0, 20.0), ("tokB", 97.0, 30.0)]).await;
    f.two_profiles();
    let output = f.run(&["--json", "auto", "--once"]);
    assert_eq!(output.status.code(), Some(3), "{}", describe(&output));
    assert_eq!(f.live_uuid(), "U1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_switches_then_runs_claude_with_passthrough_args() {
    let f = Fixture::new(&[("tokA", 50.0, 20.0), ("tokB", 10.0, 5.0)]).await;
    f.two_profiles();
    // Stand-in for `claude`: this binary answers `--version` and exits 0.
    let mut cmd = f.command(&["launch", "work", "--", "--version"]);
    cmd.env("CS_CLAUDE_BIN", env!("CARGO_BIN_EXE_paper-claude-switch"));
    let output = output_with_timeout(&mut cmd);
    success(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(env!("CARGO_PKG_VERSION")),
        "{}",
        describe(&output)
    );
    assert_eq!(f.live_uuid(), "U2");
    assert_eq!(fs::read_to_string(f.path("app/current")).unwrap(), "work");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_reports_a_missing_claude_binary_without_switching() {
    let f = Fixture::new(&[("tokA", 50.0, 20.0), ("tokB", 10.0, 5.0)]).await;
    f.two_profiles();
    let mut cmd = f.command(&["launch", "work"]);
    cmd.env("PATH", f.path("home")).env_remove("CS_CLAUDE_BIN");
    let output = output_with_timeout(&mut cmd);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("claude"), "{}", describe(&output));
    assert_eq!(f.live_uuid(), "U1");
}
