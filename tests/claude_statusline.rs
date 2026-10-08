// `statusline`: Claude Code's status line command. It reads `rate_limits` from
// stdin, stores them as a fresh usage reading of the live account, and never
// touches the network.

use std::fs;
use std::io::{Read, Write};
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

const BIN: &str = env!("CARGO_BIN_EXE_paper-claude-switch");

#[derive(Default)]
struct Calls {
    /// Bearer tokens of the usage requests.
    usage_tokens: Vec<String>,
    /// Every request that reached the mock, whatever its route.
    total: usize,
}

type Shared = Arc<Mutex<Calls>>;

struct Mock {
    base: String,
    calls: Shared,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Mock {
    async fn start() -> Self {
        let calls: Shared = Arc::default();
        let app = Router::new()
            .route("/api/oauth/usage", get(usage_handler))
            .route("/api/oauth/profile", get(other_handler))
            .route("/v1/oauth/token", post(other_handler))
            .fallback(other_handler)
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

    fn total_requests(&self) -> usize {
        self.calls.lock().unwrap().total
    }

    fn usage_requests_for(&self, token: &str) -> usize {
        self.calls.lock().unwrap().usage_tokens.iter().filter(|t| *t == token).count()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn usage_handler(State(calls): State<Shared>, headers: HeaderMap) -> Response {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_owned();
    {
        let mut calls = calls.lock().unwrap();
        calls.total += 1;
        calls.usage_tokens.push(token);
    }
    axum::Json(json!({
        "five_hour": {"utilization": 90.0, "resets_at": "2099-01-01T00:00:00Z"},
        "seven_day": {"utilization": 90.0, "resets_at": "2099-01-05T00:00:00Z"}
    }))
    .into_response()
}

async fn other_handler(State(calls): State<Shared>) -> Response {
    calls.lock().unwrap().total += 1;
    StatusCode::NOT_FOUND.into_response()
}

fn write_json(path: impl AsRef<Path>, value: &Value) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn oauth(token: &str, refresh: &str) -> Value {
    json!({
        "accessToken": token, "refreshToken": refresh,
        "expiresAt": chrono::Utc::now().timestamp_millis() + 3_600_000,
        "subscriptionType": "max"
    })
}

fn rate_limits_stdin() -> Vec<u8> {
    let now = chrono::Utc::now().timestamp();
    serde_json::to_vec(&json!({
        "rate_limits": {
            "five_hour": {"used_percentage": 37.5, "resets_at": now + 3600},
            "seven_day": {"used_percentage": 12, "resets_at": now + 86_400}
        }
    }))
    .unwrap()
}

struct Fixture {
    root: tempfile::TempDir,
    mock: Mock,
}

impl Fixture {
    /// Profiles personal (U1) and work (U2); Claude Code is logged in to `live_uuid`.
    async fn new(live_uuid: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix("claude-statusline-")
            .tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
            .unwrap();
        for dir in ["claude", "app", "home", "codex"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let f = Self { root, mock: Mock::start().await };
        let live = json!({"accountUuid": live_uuid, "emailAddress": "live@x.com"});
        write_json(f.path("claude/.credentials.json"), &json!({"claudeAiOauth": oauth("tokA", "rtA")}));
        write_json(f.path("claude/.claude.json"), &json!({"oauthAccount": live}));
        for (alias, uuid, email, token, refresh) in [
            ("personal", "U1", "a@x.com", "tokA", "rtA"),
            ("work", "U2", "b@x.com", "tokB", "rtB"),
        ] {
            let dir = f.path("app/profiles").join(alias);
            write_json(dir.join("credentials.json"), &json!({"claudeAiOauth": oauth(token, refresh)}));
            write_json(dir.join("account.json"), &json!({"accountUuid": uuid, "emailAddress": email}));
        }
        fs::write(f.path("app/current"), "personal").unwrap();
        f
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn cache_bytes(&self) -> Option<Vec<u8>> {
        fs::read(self.path("app/cache.json")).ok()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args).current_dir(self.root.path())
            .stdout(Stdio::piped()).stderr(Stdio::piped())
            .env("CLAUDE_CONFIG_DIR", self.path("claude"))
            .env("PAPER_CLAUDE_SWITCH_HOME", self.path("app"))
            .env("HOME", self.path("home"))
            .env("USERPROFILE", self.path("home"))
            .env("CODEX_HOME", self.path("codex"))
            .env("CS_CLAUDE_API_BASE", &self.mock.base)
            .env("CS_CLAUDE_TOKEN_URL", format!("{}/v1/oauth/token", self.mock.base))
            .env("CS_GITHUB_API_URL", &self.mock.base)
            .env("CS_UPDATE_TTL_SECS", "999999999")
            .env("NO_COLOR", "1");
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy",
            "all_proxy", "CS_PROXY", "RUST_LOG"] {
            cmd.env_remove(key);
        }
        cmd
    }

    /// Run the binary with `stdin` piped to it.
    fn run_with_stdin(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut cmd = self.command(args);
        cmd.stdin(Stdio::piped());
        output_with_timeout(&mut cmd, Some(stdin.to_vec()))
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = self.command(args);
        cmd.stdin(Stdio::null());
        output_with_timeout(&mut cmd, None)
    }

    /// The rows of a successful `--json list`.
    fn rows(&self) -> Vec<Value> {
        let output = self.run(&["--json", "list"]);
        assert!(output.status.success(), "status: {}; stdout: {}; stderr: {}",
            output.status, String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!(
            "invalid JSON: {error}; stdout: {}", String::from_utf8_lossy(&output.stdout)));
        value["profiles"].as_array().unwrap().clone()
    }
}

fn output_with_timeout(cmd: &mut Command, stdin: Option<Vec<u8>>) -> Output {
    let mut child = cmd.spawn().unwrap();
    let writer = stdin.map(|bytes| {
        let mut pipe = child.stdin.take().unwrap();
        thread::spawn(move || {
            // The child may exit without reading everything.
            let _ = pipe.write_all(&bytes);
        })
    });
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
            if let Some(writer) = writer {
                writer.join().unwrap();
            }
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

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// L1 + L6: the numbers land in the cache, the line shows them rounded, and
/// neither the command nor the following `list` asks the endpoint about the
/// live account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statusline_stores_the_rate_limits_and_prints_one_line() {
    let f = Fixture::new("U1").await;

    let output = f.run_with_stdin(&["statusline"], &rate_limits_stdin());

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(text(&output).trim_end(), "personal 5h 38% \u{b7} 7d 12%");
    assert_eq!(f.mock.total_requests(), 0, "statusline must not use the network");

    let rows = f.rows();
    assert_eq!(rows[0]["alias"], "personal");
    assert_eq!(rows[0]["usage"]["primary"]["used_percent"], 37.5, "{}", rows[0]);
    assert_eq!(rows[0]["usage"]["secondary"]["used_percent"], 12.0, "{}", rows[0]);
    assert_eq!(f.mock.usage_requests_for("tokA"), 0, "the live account is served from the cache");
}

/// L2: with `-- <cmd>` the next command's stdout and exit code are ours, and
/// the cache is still written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statusline_hands_over_to_the_next_command() {
    let f = Fixture::new("U1").await;
    let expected = f.run(&["--version"]);
    assert!(expected.status.success());
    assert!(!expected.stdout.is_empty());

    let output = f.run_with_stdin(&["statusline", "--", BIN, "--version"], &rate_limits_stdin());

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, expected.stdout, "the next command's stdout, unchanged");
    assert_eq!(f.mock.total_requests(), 0);
    let rows = f.rows();
    assert_eq!(rows[0]["usage"]["primary"]["used_percent"], 37.5, "{}", rows[0]);

    let failing = f.run(&["--no-such-flag"]);
    assert!(!failing.status.success());
    let output = f.run_with_stdin(&["statusline", "--", BIN, "--no-such-flag"], &rate_limits_stdin());
    assert_eq!(output.status.code(), failing.status.code(), "the next command's exit code");
    assert_eq!(output.stdout, failing.stdout);
}

/// L3: stdin without rate_limits changes nothing in the cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statusline_without_rate_limits_leaves_the_cache_alone() {
    let f = Fixture::new("U1").await;
    fs::write(f.path("app/cache.json"), br#"{"entries":{}}"#).unwrap();
    let before = f.cache_bytes();

    let output = f.run_with_stdin(&["statusline"], b"{}");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(f.cache_bytes(), before, "cache.json must be byte-for-byte unchanged");
    assert_eq!(f.mock.total_requests(), 0);
}

/// L4: stdin that is not JSON never breaks the status line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statusline_survives_garbage_on_stdin() {
    let f = Fixture::new("U1").await;

    let output = f.run_with_stdin(&["statusline"], b"not json");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stdout.is_empty(), "stdout: {}", text(&output));
    assert_eq!(f.cache_bytes(), None);
    assert_eq!(f.mock.total_requests(), 0);
}

/// L5: a live login without a saved profile prints nothing and caches nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statusline_ignores_an_unsaved_account() {
    let f = Fixture::new("U9").await;

    let output = f.run_with_stdin(&["statusline"], &rate_limits_stdin());

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stdout.is_empty(), "stdout: {}", text(&output));
    assert_eq!(f.cache_bytes(), None, "no cache entry for an unknown account");
    assert_eq!(f.mock.total_requests(), 0);
}

/// `--tee`: the numbers are stored and stdin comes back out byte for byte, so
/// the command can sit in front of another status line in a shell pipe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statusline_tee_stores_the_rate_limits_and_echoes_stdin() {
    let f = Fixture::new("U1").await;
    let input = rate_limits_stdin();

    let output = f.run_with_stdin(&["statusline", "--tee"], &input);

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, input, "stdin is echoed unchanged");
    assert_eq!(f.mock.total_requests(), 0);
    let rows = f.rows();
    assert_eq!(rows[0]["usage"]["primary"]["used_percent"], 37.5, "{}", rows[0]);
}

/// `--tee` with input it cannot use still echoes it unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statusline_tee_echoes_garbage_unchanged() {
    let f = Fixture::new("U1").await;
    let output = f.run_with_stdin(&["statusline", "--tee"], b"not json");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"not json");
}
