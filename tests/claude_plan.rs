// The plan name and the exact "Plan until" date come from what Claude keeps
// locally (account.json) plus the profile endpoint's subscription status.

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
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use claude_switch::claude_api::{Endpoints, ProfileStatus, fetch_profile};
use claude_switch::claude_usage::{next_renewal, plan_label};
use serde_json::{Value, json};

fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0).unwrap().timestamp()
}

// ── pure functions ───────────────────────────────────────

/// P1: the renewal keeps the creation day and time of day, seconds only.
#[test]
fn next_renewal_is_the_next_monthly_creation_day() {
    let now = at(2026, 10, 8, 0, 0);
    assert_eq!(
        next_renewal("2025-07-20T06:35:00.366209Z", now),
        Some(at(2026, 10, 20, 6, 35))
    );
}

/// P2: a day that a shorter month lacks falls back to that month's last day.
#[test]
fn next_renewal_clamps_to_the_last_day_of_short_months() {
    assert_eq!(
        next_renewal("2025-01-31T10:00:00Z", at(2026, 2, 10, 0, 0)),
        Some(at(2026, 2, 28, 10, 0))
    );
    assert_eq!(
        next_renewal("2025-01-31T10:00:00Z", at(2026, 3, 1, 0, 0)),
        Some(at(2026, 3, 31, 10, 0))
    );
}

/// P3: at the renewal instant itself the next renewal is a month later.
#[test]
fn next_renewal_is_strictly_after_now() {
    assert_eq!(
        next_renewal("2025-07-20T06:35:00Z", at(2026, 10, 20, 6, 35)),
        Some(at(2026, 11, 20, 6, 35))
    );
}

/// P4
#[test]
fn next_renewal_of_garbage_is_none() {
    assert_eq!(next_renewal("not a date", at(2026, 10, 8, 0, 0)), None);
}

/// P5
#[test]
fn plan_label_names_the_plan() {
    assert_eq!(plan_label(Some("default_claude_max_5x"), None, None), "max5x");
    assert_eq!(plan_label(Some("default_claude_max_20x"), None, None), "max20x");
    assert_eq!(plan_label(None, Some("claude_pro"), None), "pro");
    assert_eq!(plan_label(None, None, Some("team")), "team");
    assert_eq!(plan_label(None, None, None), "--");
}

// ── mock server ──────────────────────────────────────────

#[derive(Default)]
struct Calls {
    usage_tokens: Vec<String>,
    /// (bearer token, anthropic-beta header) of every profile request.
    profile_requests: Vec<(String, String)>,
}

#[derive(Clone)]
struct MockState {
    profile_fails: bool,
    calls: Arc<Mutex<Calls>>,
}

struct Mock {
    base: String,
    calls: Arc<Mutex<Calls>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Mock {
    async fn start(profile_fails: bool) -> Self {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let app = Router::new()
            .route("/api/oauth/usage", get(usage_handler))
            .route("/api/oauth/profile", get(profile_handler))
            .route("/v1/oauth/token", post(|| async { StatusCode::UNAUTHORIZED }))
            .with_state(MockState { profile_fails, calls: calls.clone() });
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

    fn profile_requests(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().profile_requests.clone()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_owned()
}

async fn usage_handler(State(state): State<MockState>, headers: HeaderMap) -> Response {
    let token = bearer(&headers);
    state.calls.lock().unwrap().usage_tokens.push(token.clone());
    if token != "tokA" && token != "tokB" {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    axum::Json(json!({
        "five_hour": {"utilization": 30.0, "resets_at": "2099-01-01T00:00:00Z"},
        "seven_day": {"utilization": 5.0, "resets_at": "2099-01-05T00:00:00Z"}
    }))
    .into_response()
}

async fn profile_handler(State(state): State<MockState>, headers: HeaderMap) -> Response {
    let token = bearer(&headers);
    let beta = headers
        .get("anthropic-beta")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    state.calls.lock().unwrap().profile_requests.push((token.clone(), beta));
    if state.profile_fails {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let status = if token == "tokB" { "canceled" } else { "active" };
    axum::Json(json!({
        "account": {"uuid": "U1", "email": "a@x.com"},
        "organization": {
            "uuid": "O1",
            "organization_type": "claude_max",
            "rate_limit_tier": "default_claude_max_5x",
            "subscription_status": status,
            "subscription_created_at": "2025-07-20T06:35:00.366209Z"
        }
    }))
    .into_response()
}

// ── P6: fetch_profile ────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_profile_reads_the_organization_status() {
    let mock = Mock::start(false).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let endpoints = Endpoints {
        api_base: mock.base.clone(),
        token_url: format!("{}/v1/oauth/token", mock.base),
    };

    let status = fetch_profile(&client, &endpoints, "tokB")
        .await
        .unwrap_or_else(|e| panic!("fetch_profile failed: {e:?}"));

    assert_eq!(
        status,
        ProfileStatus {
            subscription_status: Some("canceled".to_owned()),
            subscription_created_at: Some("2025-07-20T06:35:00.366209Z".to_owned()),
            rate_limit_tier: Some("default_claude_max_5x".to_owned()),
            organization_type: Some("claude_max".to_owned()),
        }
    );
    let requests = mock.profile_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "tokB", "Bearer token");
    assert!(!requests[0].1.is_empty(), "anthropic-beta header");
}

// ── P7-P9: the CLI ───────────────────────────────────────

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

fn account(uuid: &str, email: &str, tier: &str, created: &str) -> Value {
    json!({
        "accountUuid": uuid, "emailAddress": email,
        "billingType": "stripe_subscription",
        "subscriptionCreatedAt": created,
        "organizationRateLimitTier": tier,
        "organizationType": "claude_max"
    })
}

const PERSONAL_CREATED: &str = "2025-07-20T06:35:00Z";
const WORK_CREATED: &str = "2025-01-31T10:00:00Z";

/// Independent calendar arithmetic for the expected renewal: the creation day
/// and time of day, clamped to the length of the month, first one after now.
fn expected_renewal(created: &str, now: i64) -> i64 {
    let created = DateTime::parse_from_rfc3339(created).unwrap().with_timezone(&Utc);
    for months in 1..=1200_u32 {
        let index = created.year() * 12 + created.month0() as i32 + months as i32;
        let (year, month) = (index.div_euclid(12), index.rem_euclid(12) as u32 + 1);
        let first_of_next = if month == 12 {
            NaiveDate::from_ymd_opt(year + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1)
        }
        .unwrap();
        let days_in_month = first_of_next.pred_opt().unwrap().day();
        let date = NaiveDate::from_ymd_opt(year, month, created.day().min(days_in_month)).unwrap();
        let instant = date.and_time(created.time()).and_utc().timestamp();
        if instant > now {
            return instant;
        }
    }
    panic!("no renewal found");
}

struct Fixture {
    root: tempfile::TempDir,
    mock: Mock,
}

impl Fixture {
    async fn new(profile_fails: bool) -> Self {
        let root = tempfile::Builder::new()
            .prefix("claude-plan-")
            .tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
            .unwrap();
        for dir in ["claude", "app", "home", "codex"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let f = Self { root, mock: Mock::start(profile_fails).await };
        let personal = account("U1", "a@x.com", "default_claude_max_5x", PERSONAL_CREATED);
        let work = account("U2", "b@x.com", "default_claude_max_20x", WORK_CREATED);
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

    /// The rows of a successful `--json list`.
    fn rows(&self, args: &[&str]) -> Vec<Value> {
        let output = self.run(args);
        assert!(output.status.success(), "status: {}; stdout: {}; stderr: {}",
            output.status, String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!(
            "invalid JSON: {error}; stdout: {}", String::from_utf8_lossy(&output.stdout)));
        value["profiles"].as_array().unwrap().clone()
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

/// P7: plan name and renewal come from account.json, the status from the
/// profile endpoint, each with the token of its own account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_shows_plan_renewal_and_subscription_status() {
    let f = Fixture::new(false).await;
    let rows = f.rows(&["--json", "list"]);
    let now = chrono::Utc::now().timestamp();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["alias"], "personal");
    assert_eq!(rows[0]["account"]["plan"], "max5x");
    assert_eq!(rows[0]["account"]["subscription_until"], expected_renewal(PERSONAL_CREATED, now));
    assert_eq!(rows[0]["account"]["subscription_status"], "active");
    assert_eq!(rows[1]["alias"], "work");
    assert_eq!(rows[1]["account"]["plan"], "max20x");
    assert_eq!(rows[1]["account"]["subscription_until"], expected_renewal(WORK_CREATED, now));
    assert_eq!(rows[1]["account"]["subscription_status"], "canceled");
    assert_eq!(rows[0]["usage"]["primary"]["used_percent"], 30.0);
    assert_eq!(rows[1]["usage"]["primary"]["used_percent"], 30.0);

    let mut tokens: Vec<String> = f.mock.profile_requests().into_iter().map(|r| r.0).collect();
    tokens.sort();
    assert_eq!(tokens, ["tokA", "tokB"], "each account's profile is read with its own token");
}

/// P8: the status is cached for hours, so a second forced list does not ask again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_status_is_fetched_once_per_account_in_six_hours() {
    let f = Fixture::new(false).await;
    f.rows(&["--json", "list", "--force"]);
    let rows = f.rows(&["--json", "list", "--force"]);

    assert_eq!(f.mock.profile_requests().len(), 2, "one profile request per account in total");
    assert_eq!(rows[0]["account"]["subscription_status"], "active");
    assert_eq!(rows[1]["account"]["subscription_status"], "canceled");
}

/// P9: a failing profile endpoint never fails the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_endpoint_failure_does_not_fail_the_rows() {
    let f = Fixture::new(true).await;
    let rows = f.rows(&["--json", "list"]);

    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row["usage"]["primary"]["used_percent"], 30.0, "{row}");
        let account = row["account"].as_object().unwrap();
        assert_eq!(account.get("subscription_status"), Some(&Value::Null), "{row}");
    }
}
