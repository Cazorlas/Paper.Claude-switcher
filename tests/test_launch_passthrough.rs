//! End-to-end argv contract for `codex-switch launch -- …` and for the Codex
//! app-server daemon handling of `codex-switch use`.
//!
//! A fake `codex` on PATH records the exact argument vector it received, so
//! these tests prove the composed command rather than only the clap parse.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
#[cfg(any(unix, windows))]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use serde_json::Value;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_home(name: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("codex-switch-launch-{name}-{ts}-{id}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn jwt(payload: &Value) -> String {
    let json = serde_json::to_vec(payload).unwrap();
    let encoded = {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        URL_SAFE_NO_PAD.encode(json)
    };
    format!("x.{encoded}.y")
}

fn write_auth(path: &Path, email: &str, account_id: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let claims = serde_json::json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "plus",
            "chatgpt_account_id": account_id,
            "chatgpt_user_id": format!("user_{account_id}"),
            "organizations": [],
        }
    });
    let auth = serde_json::json!({
        "tokens": {
            "id_token": jwt(&claims),
            "refresh_token": "dummy-refresh",
            "access_token": "dummy-access",
            "account_id": account_id,
        },
        "last_refresh": "2026-08-01T00:00:00Z",
    });
    fs::write(path, serde_json::to_string_pretty(&auth).unwrap()).unwrap();
}

const FAKE_CODEX_PY: &str = r#"import json, os, sys
path = os.environ["CS_FAKE_CODEX_LOG"]
try:
    data = json.loads(open(path, encoding="utf-8").read())
except Exception:
    data = []
data.append({
    "argv": sys.argv[1:],
    "pid": os.getpid(),
    "codex_home": os.environ.get("CODEX_HOME"),
})
# Write to a temp file and rename it into place so a concurrent reader never
# sees a truncated, half-written log.
tmp_path = "%s.%d.tmp" % (path, os.getpid())
with open(tmp_path, "w", encoding="utf-8") as tmp:
    tmp.write(json.dumps(data))
for attempt in range(50):
    try:
        os.replace(tmp_path, path)
        break
    except PermissionError:
        # Windows refuses the rename while a reader briefly holds the target.
        import time
        time.sleep(0.02)
else:
    os.replace(tmp_path, path)

argv = sys.argv[1:]
if argv == ["--version"]:
    if os.environ.get("CS_FAKE_CODEX_VERSION_DELAY"):
        import time
        time.sleep(float(os.environ["CS_FAKE_CODEX_VERSION_DELAY"]))
    if os.environ.get("CS_FAKE_CODEX_VERSION_FAIL") == "1":
        sys.exit(7)
    sys.stdout.write("codex-cli " + os.environ.get("CS_FAKE_CODEX_VERSION", "0.159.2") + "\n")
    sys.exit(0)
if argv == ["--help"]:
    if os.environ.get("CS_FAKE_CODEX_HELP_DELAY"):
        import time
        time.sleep(float(os.environ["CS_FAKE_CODEX_HELP_DELAY"]))
    if os.environ.get("CS_FAKE_CODEX_HELP_FAIL") == "1":
        sys.exit(1)
    # Codex 0.156+ lists `--no-daemon` in its root help.
    sys.stdout.write("Usage: codex [OPTIONS] [PROMPT]\n")
    if os.environ.get("CS_FAKE_CODEX_NO_DAEMON") == "1":
        sys.stdout.write("      --no-daemon\n")
    sys.exit(0)
if argv == ["app-server", "daemon", "version"]:
    if os.environ.get("CS_FAKE_CODEX_DAEMON") == "running":
        sys.stdout.write('{"status":"running","cliVersion":"0.159.2","appServerVersion":"0.159.2"}\n')
        sys.exit(0)
    sys.stderr.write("Error: failed to connect to app-server-control.sock\n")
    sys.exit(1)
if argv == ["app-server", "daemon", "restart"]:
    if os.environ.get("CS_FAKE_CODEX_DAEMON_RESTART") == "fail":
        sys.stderr.write("Error: app server is running but is not managed by codex app-server daemon\n")
        sys.exit(1)
    sys.stdout.write('{"status":"restarted"}\n')
    sys.exit(0)

# A real Codex invocation creates its session index and rollout under the
# CODEX_HOME it received.  The fixture is opt-in so the older argv-only tests
# keep exercising the same small fake.
session_id = os.environ.get("CS_FAKE_CODEX_SESSION_ID")
if session_id:
    codex_home = os.environ["CODEX_HOME"]
    session_name = os.environ.get("CS_FAKE_CODEX_SESSION_NAME", session_id)
    provider = os.environ.get("CS_FAKE_CODEX_SESSION_PROVIDER", "openrouter")
    for arg in sys.argv[1:]:
        if arg.startswith("model_provider="):
            provider = arg.split("=", 1)[1].strip('"')
    model = os.environ.get("CS_FAKE_CODEX_SESSION_MODEL", "openai/gpt-5.3-codex")
    updated_at = os.environ.get("CS_FAKE_CODEX_SESSION_UPDATED_AT", "2026-09-09T00:00:00Z")
    day = os.path.join(codex_home, "sessions", "2026", "09", "09")
    os.makedirs(day, exist_ok=True)
    rollout = os.path.join(day, "rollout-" + session_id + ".jsonl")
    meta = {
        "type": "session_meta",
        "id": session_id,
        "name": session_name,
        "model_provider": provider,
        "model": model,
        "updated_at": updated_at,
    }
    with open(rollout, "w", encoding="utf-8") as stream:
        stream.write(json.dumps(meta) + "\n")
    index = os.path.join(codex_home, "session_index.jsonl")
    with open(index, "a", encoding="utf-8") as stream:
        stream.write(json.dumps({
            "id": session_id,
            "name": session_name,
            "thread_name": session_name,
            "model_provider": provider,
            "model": model,
            "updated_at": updated_at,
            "rollout_path": os.path.relpath(rollout, codex_home).replace(os.sep, "/"),
        }) + "\n")
delay = float(os.environ.get("CS_FAKE_CODEX_SLEEP", "0"))
if delay:
    import time
    time.sleep(delay)
size = int(os.environ.get("CS_FAKE_CODEX_STDOUT_BYTES", "0"))
sys.stdout.write("x" * size if size else "codex-ok\n")
sys.stdout.flush()
if os.environ.get("CS_FAKE_CODEX_DONE"):
    open(os.environ["CS_FAKE_CODEX_DONE"], "w").write("completed")
sys.exit(0)
"#;

const ARGV_EDGE_CASE: &str = "review with spaces 世界 & echo should-not-run";

#[cfg(unix)]
fn locate_python3() -> &'static Path {
    static PYTHON3: OnceLock<PathBuf> = OnceLock::new();
    PYTHON3.get_or_init(|| {
        let paths = std::env::var_os("PATH").unwrap_or_default();
        let python = std::env::split_paths(&paths)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| panic!("Unix launch tests require python3 on the test runner PATH"))
            .canonicalize()
            .unwrap_or_else(|error| panic!("resolving the test runner's python3: {error}"));

        // Resolve and start the test interpreter before launching codex-switch:
        // the CLI's first version probe has a strict four-second deadline.
        let output = Command::new(&python)
            .args(["-c", "pass"])
            .output()
            .unwrap_or_else(|error| panic!("starting the test runner's python3: {error}"));
        assert!(
            output.status.success(),
            "test runner python3 prewarm failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        python
    })
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

#[cfg(windows)]
fn locate_python() -> &'static Path {
    static PYTHON: OnceLock<PathBuf> = OnceLock::new();
    PYTHON.get_or_init(|| {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let candidate = std::env::split_paths(&path)
            .flat_map(|dir| ["python.exe", "python3.exe", "py.exe"].map(|name| dir.join(name)))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| {
                panic!("Windows launch tests require an existing Python executable on PATH")
            });
        if candidate.is_absolute() {
            candidate
        } else {
            std::env::current_dir()
                .unwrap_or_else(|error| {
                    panic!("resolving the test runner's current directory: {error}")
                })
                .join(candidate)
        }
    })
}

fn warm_fake_codex(home: &Path, fake_bin: &Path, log: &Path) {
    #[cfg(unix)]
    let mut command = Command::new(fake_bin.join("codex"));
    #[cfg(unix)]
    command.arg("--version");
    #[cfg(windows)]
    let mut command = Command::new(fake_bin.join("codex.cmd"));
    #[cfg(windows)]
    command.arg("--version");
    command
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("PAPER_CLAUDE_SWITCH_HOME", home.join(".paper-claude-switch"));
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("CS_FAKE_CODEX_")
        {
            command.env_remove(name);
        }
    }
    command
        .env("CS_FAKE_CODEX_LOG", log)
        .env("CS_FAKE_CODEX_VERSION", "0.159.2");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("warming fake Codex executable: {error}"));
    assert!(
        output.status.success(),
        "fake Codex warmup failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(log, "[]").unwrap();
}

fn install_fake_codex(home: &Path) -> (PathBuf, PathBuf) {
    let bin_dir = home.join("fake-bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let log = home.join("fake-codex-log.json");
    fs::write(&log, "[]").unwrap();
    #[cfg(unix)]
    {
        let python = locate_python3();
        let fake_script = bin_dir.join("fake_codex.py");
        fs::write(&fake_script, FAKE_CODEX_PY).unwrap();
        let script = bin_dir.join("codex");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nexec {} {} \"$@\"\n",
                shell_quote(python),
                shell_quote(&fake_script)
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();
    }
    #[cfg(windows)]
    {
        let script = bin_dir.join("fake_codex.py");
        fs::write(&script, FAKE_CODEX_PY).unwrap();
        let python = locate_python();
        let python = python.to_string_lossy().replace('"', "\"\"");
        fs::write(
            bin_dir.join("codex.cmd"),
            format!("@echo off\r\n\"{python}\" \"%~dp0fake_codex.py\" %*\r\n"),
        )
        .unwrap();
    }
    warm_fake_codex(home, &bin_dir, &log);
    (bin_dir, log)
}

/// Argv of a codex-switch probe (`--version`, `--help`, `app-server daemon …`)
/// rather than a launched Codex session.
fn is_probe(argv: &[String]) -> bool {
    matches!(argv, [flag] if flag == "--version" || flag == "--help")
        || argv.first().is_some_and(|first| first == "app-server")
}

fn recorded_argv(log: &Path) -> Vec<Vec<String>> {
    let raw = fs::read_to_string(log).unwrap();
    let data: Vec<Value> = serde_json::from_str(&raw).unwrap();
    data.into_iter()
        .map(|entry| {
            entry["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        })
        .collect()
}

fn last_non_version_argv(log: &Path) -> Vec<String> {
    recorded_argv(log)
        .into_iter()
        .rev()
        .find(|argv| !is_probe(argv))
        .expect("fake codex must have been launched with real args")
}

fn command(home: &Path, fake_bin: &Path, log: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_paper-claude-switch"));
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.env("HOME", home);
    cmd.env("CODEX_HOME", home.join(".codex"));
    cmd.env("PAPER_CLAUDE_SWITCH_HOME", home.join(".paper-claude-switch"));
    cmd.env("CS_FAKE_CODEX_LOG", log);
    #[cfg(unix)]
    cmd.env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()));
    #[cfg(windows)]
    {
        let mut paths = vec![fake_bin.to_path_buf()];
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let system_root = PathBuf::from(system_root);
            paths.push(system_root.join("System32"));
            paths.push(system_root);
        }
        cmd.env("PATH", std::env::join_paths(paths).unwrap());
    }
    cmd.env_remove("HTTP_PROXY");
    cmd.env_remove("HTTPS_PROXY");
    cmd.env_remove("ALL_PROXY");
    cmd.env_remove("CS_PROXY");
    cmd
}

fn run(home: &Path, fake_bin: &Path, log: &Path, args: &[&str]) -> Output {
    command(home, fake_bin, log, args).output().unwrap()
}

fn run_env(
    home: &Path,
    fake_bin: &Path,
    log: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Output {
    let mut cmd = command(home, fake_bin, log, args);
    for (name, value) in env {
        cmd.env(name, value);
    }
    cmd.output().unwrap()
}

fn setup_chatgpt(home: &Path) {
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        home.join(".codex/config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )
    .unwrap();
    fs::create_dir_all(home.join(".paper-claude-switch")).unwrap();
    fs::write(
        home.join(".paper-claude-switch/config.toml"),
        "[launch]\nrestore_delay_secs = 1\n",
    )
    .unwrap();
    write_auth(
        &home.join(".paper-claude-switch/profiles/work/auth.json"),
        "work@example.com",
        "acct_work",
    );
    fs::write(home.join(".paper-claude-switch/current"), "work").unwrap();
}

#[test]
fn launch_dash_dash_exec_json_is_not_an_alias_named_exec() {
    let home = temp_home("dash-dash-exec");
    let (fake_bin, log) = install_fake_codex(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "--", "exec", "--json", "review this"],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "auto-select with no profiles must fail: {combined}"
    );
    assert!(
        combined.contains("no saved profiles"),
        "launch -- exec must auto-select, not look up alias exec: {combined}"
    );
    assert!(
        !combined.contains("profile 'exec' not found"),
        "launch -- exec must not treat exec as an alias: {combined}"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_puts_cs_model_after_exec() {
    let home = temp_home("chatgpt-model-exec");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &[
            "launch",
            "work",
            "--model",
            "gpt-5.4",
            "--",
            "exec",
            "--json",
            ARGV_EDGE_CASE,
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        last_non_version_argv(&log),
        ["exec", "--model", "gpt-5.4", "--json", ARGV_EDGE_CASE]
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_exec_without_separator_is_not_an_alias() {
    let home = temp_home("exec-not-alias");
    let (fake_bin, log) = install_fake_codex(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "exec", "--json", "review this"],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "auto-select with no profiles must fail: {combined}"
    );
    assert!(
        combined.contains("no saved profiles"),
        "launch exec must auto-select, not look up alias exec: {combined}"
    );
    assert!(!combined.contains("profile 'exec' not found"), "{combined}");
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_merges_tokens_on_both_sides_of_double_dash() {
    let home = temp_home("merge-dash");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "exec", "--", "--json", "hi"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(last_non_version_argv(&log), ["exec", "--json", "hi"]);
    let _ = fs::remove_dir_all(home);
}

fn strings(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|arg| arg.to_string()).collect()
}

/// Every `codex app-server …` invocation the fake recorded, in order.
#[test]
fn launch_chatgpt_runs_codex_without_the_shared_daemon_when_supported() {
    let home = temp_home("launch-no-daemon");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "--", "exec", "--json", "review"],
        &[("CS_FAKE_CODEX_NO_DAEMON", "1")],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        last_non_version_argv(&log),
        ["--no-daemon", "exec", "--json", "review"]
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_accepts_a_slow_valid_help_probe() {
    let home = temp_home("launch-slow-help");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);
    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "--", "exec", "review"],
        &[
            ("CS_FAKE_CODEX_HELP_DELAY", "3"),
            ("CS_FAKE_CODEX_NO_DAEMON", "1"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        last_non_version_argv(&log),
        ["--no-daemon", "exec", "review"]
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_rejects_failed_help_before_staging_credentials() {
    let home = temp_home("launch-failed-help");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);
    let live_auth = home.join(".codex/auth.json");
    write_auth(&live_auth, "original@example.com", "acct_original");
    let original = fs::read(&live_auth).unwrap();
    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "--", "exec", "review"],
        &[("CS_FAKE_CODEX_HELP_FAIL", "1")],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("account routing is unknown"));
    assert_eq!(fs::read(live_auth).unwrap(), original);
    assert!(recorded_argv(&log).iter().all(|argv| is_probe(argv)));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_keeps_argv_for_a_codex_without_no_daemon() {
    let home = temp_home("launch-old-codex");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "--", "exec", "--json", "review"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(last_non_version_argv(&log), ["exec", "--json", "review"]);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_leaves_an_explicit_server_choice_alone() {
    let home = temp_home("launch-explicit-server");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    for passthrough in [
        vec!["--remote", "ws://127.0.0.1:1"],
        vec!["--no-daemon", "hello"],
    ] {
        let mut args = vec!["launch", "work", "--"];
        args.extend(passthrough.iter().copied());
        let output = run_env(
            &home,
            &fake_bin,
            &log,
            &args,
            &[("CS_FAKE_CODEX_NO_DAEMON", "1")],
        );
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(last_non_version_argv(&log), strings(&passthrough));
    }
    let _ = fs::remove_dir_all(home);
}
