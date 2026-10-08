// Claude's "reset your session limit" offer (`juniper_tide` in the usage reply):
// parsed by parse_usage, kept in the cache, shown by `list` as a Reset column.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use claude_switch::claude_api::{SessionReset, parse_usage};
use serde_json::{Value, json};

fn available_block() -> Value {
    json!({
        "eligible": true,
        "available": true,
        "resets_per_week": 2,
        "event_props": {"billing_period": "monthly"}
    })
}

fn usage_body(juniper_tide: Value) -> Value {
    json!({
        "five_hour": {"utilization": 10.0, "resets_at": "2099-01-01T00:00:00Z"},
        "juniper_tide": juniper_tide
    })
}

// ── S1-S3: parse_usage ───────────────────────────────────

/// S1
#[test]
fn an_available_offer_is_parsed() {
    let usage = parse_usage(&usage_body(available_block())).expect("usable usage");

    assert_eq!(
        usage.session_reset,
        Some(SessionReset {
            eligible: true,
            ineligible_reason: None,
            available: true,
            next_available_at: None,
            resets_per_week: 2,
            billing_period: Some("monthly".to_owned()),
        })
    );
}

/// S2
#[test]
fn a_used_offer_carries_its_next_time_and_defaults_to_one_reset_a_week() {
    let usage = parse_usage(&usage_body(json!({
        "eligible": true,
        "available": false,
        "next_available_at": "2026-10-12T09:00:00Z"
    })))
    .expect("usable usage");

    let reset = usage.session_reset.expect("session_reset");
    assert!(reset.eligible);
    assert!(!reset.available);
    assert_eq!(reset.next_available_at.as_deref(), Some("2026-10-12T09:00:00Z"));
    assert_eq!(reset.resets_per_week, 1);
}

/// S3
#[test]
fn a_null_or_malformed_offer_is_ignored() {
    for block in [Value::Null, json!("garbage")] {
        let usage = parse_usage(&usage_body(block.clone())).expect("usable usage");
        assert_eq!(usage.session_reset, None, "juniper_tide: {block}");
        assert_eq!(usage.five_hour.expect("five_hour").pct, 10.0, "juniper_tide: {block}");
    }
    let without = parse_usage(&json!({
        "five_hour": {"utilization": 10.0, "resets_at": "2099-01-01T00:00:00Z"}
    }))
    .expect("usable usage");
    assert_eq!(without.session_reset, None);
    assert!(parse_usage(&json!({})).is_none(), "an empty reply stays unusable");
    // Ignoring a bad block must not mean ignoring every block.
    let valid = parse_usage(&usage_body(available_block())).expect("usable usage");
    assert!(valid.session_reset.is_some(), "a well-formed block is kept");
}

// ── S4-S5: the CLI ───────────────────────────────────────

#[derive(Clone)]
struct MockState {
    token_a_body: Value,
}

struct Mock {
    base: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Mock {
    async fn start(token_a_body: Value) -> Self {
        let app = Router::new()
            .route("/api/oauth/usage", get(usage_handler))
            .route("/v1/oauth/token", post(|| async { StatusCode::UNAUTHORIZED }))
            .with_state(MockState { token_a_body });
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
        Self { base, shutdown: Some(shutdown) }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn usage_handler(State(state): State<MockState>, headers: HeaderMap) -> Response {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    match token {
        "tokA" => axum::Json(state.token_a_body).into_response(),
        "tokB" => axum::Json(json!({
            "five_hour": {"utilization": 30.0, "resets_at": "2099-01-01T00:00:00Z"},
            "seven_day": {"utilization": 5.0, "resets_at": "2099-01-05T00:00:00Z"}
        }))
        .into_response(),
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
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

struct Fixture {
    root: tempfile::TempDir,
    mock: Mock,
}

impl Fixture {
    async fn new(token_a_body: Value) -> Self {
        let root = tempfile::Builder::new()
            .prefix("claude-reset-")
            .tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
            .unwrap();
        for dir in ["claude", "app", "home", "codex"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let f = Self { root, mock: Mock::start(token_a_body).await };
        let personal = json!({"accountUuid": "U1", "emailAddress": "a@x.com"});
        let work = json!({"accountUuid": "U2", "emailAddress": "b@x.com"});
        write_json(f.path("claude/.credentials.json"), &json!({"claudeAiOauth": oauth("tokA", "rtA")}));
        write_json(f.path("claude/.claude.json"), &json!({"oauthAccount": personal}));
        for (alias, token, refresh, account) in
            [("personal", "tokA", "rtA", &personal), ("work", "tokB", "rtB", &work)]
        {
            let dir = f.path("app/profiles").join(alias);
            write_json(dir.join("credentials.json"), &json!({"claudeAiOauth": oauth(token, refresh)}));
            write_json(dir.join("account.json"), account);
        }
        fs::write(f.path("app/current"), "personal").unwrap();
        f
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn run(&self, args: &[&str]) -> Output {
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
            .env("CS_GITHUB_API_URL", &self.mock.base)
            .env("CS_UPDATE_TTL_SECS", "999999999")
            .env("NO_COLOR", "1");
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy",
            "all_proxy", "CS_PROXY", "RUST_LOG"] {
            cmd.env_remove(key);
        }
        output_with_timeout(&mut cmd)
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

/// S4: `--json list` carries the offer of an account that has one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_list_shows_the_session_reset() {
    let f = Fixture::new(usage_body(available_block())).await;

    let output = f.run(&["--json", "list"]);

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!(
        "invalid JSON: {error}; stdout: {}", String::from_utf8_lossy(&output.stdout)));
    let rows = value["profiles"].as_array().unwrap();
    assert_eq!(rows[0]["alias"], "personal");
    let reset = &rows[0]["usage"]["session_reset"];
    assert_eq!(reset["eligible"], true, "{reset}");
    assert_eq!(reset["available"], true, "{reset}");
    assert_eq!(reset["resets_per_week"], 2, "{reset}");
    assert_eq!(reset["next_available_at"], Value::Null, "{reset}");
    assert_eq!(rows[1]["usage"]["session_reset"], Value::Null, "an account without an offer");
}

/// S5: the human `list` has a Reset column saying `ready` and `N/wk`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_list_shows_ready_and_resets_per_week() {
    let f = Fixture::new(usage_body(available_block())).await;

    let output = f.run(&["list"]);

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("ready"), "stdout: {text}");
    assert!(text.contains("2/wk"), "stdout: {text}");
}
