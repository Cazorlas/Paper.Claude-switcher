use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_home(name: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("codex-switch-{name}-{ts}-{id}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_json(path: impl AsRef<Path>, value: &Value) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

fn write_cache_entry(
    home: &Path,
    alias: &str,
    ts: u64,
    primary_used: Option<f64>,
    primary_reset: Option<i64>,
) {
    let cache = serde_json::json!({
        "entries": {
            alias: {
                "ts": ts,
                "primary_used": primary_used,
                "primary_reset": primary_reset,
                "secondary_used": null,
                "secondary_reset": null
            }
        }
    });
    write_json(home.join(".paper-claude-switch/cache.json"), &cache);
}

/// Claude OAuth block whose access token is valid until the year 2100, so
/// listing never tries to refresh it.
fn claude_oauth(token: &str) -> Value {
    serde_json::json!({
        "accessToken": token,
        "refreshToken": format!("refresh-{token}"),
        "expiresAt": 4_102_444_800_000_i64,
        "subscriptionType": "pro"
    })
}

fn claude_account(email: &str, uuid: &str) -> Value {
    serde_json::json!({"accountUuid": uuid, "emailAddress": email})
}

/// The account Claude Code is logged in to (CLAUDE_CONFIG_DIR is `<home>/.claude`).
fn write_live_login(home: &Path, email: &str, uuid: &str, token: &str) {
    write_json(
        home.join(".claude/.credentials.json"),
        &serde_json::json!({"claudeAiOauth": claude_oauth(token)}),
    );
    write_json(
        home.join(".claude/.claude.json"),
        &serde_json::json!({"oauthAccount": claude_account(email, uuid)}),
    );
}

fn write_claude_profile(home: &Path, alias: &str, email: &str, uuid: &str, token: &str) {
    let dir = home.join(".paper-claude-switch/profiles").join(alias);
    write_json(
        dir.join("credentials.json"),
        &serde_json::json!({"claudeAiOauth": claude_oauth(token)}),
    );
    write_json(dir.join("account.json"), &claude_account(email, uuid));
}

/// A local endpoint that answers every request with 401, so usage lookups
/// fail fast and never leave the machine.
fn dead_endpoint() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let mut stream = stream;
                    let mut seen = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => seen.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let _ = stream.write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                });
            }
        });
        base
    })
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_paper-claude-switch")
}

fn command(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(binary());
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.env("HOME", home);
    cmd.env("USERPROFILE", home);
    cmd.env("CODEX_HOME", home.join(".codex"));
    cmd.env("CLAUDE_CONFIG_DIR", home.join(".claude"));
    cmd.env("CS_CLAUDE_API_BASE", dead_endpoint());
    cmd.env("CS_CLAUDE_TOKEN_URL", format!("{}/token", dead_endpoint()));
    cmd.env("NO_COLOR", "1");
    cmd.env("PAPER_CLAUDE_SWITCH_HOME", home.join(".paper-claude-switch"));
    cmd.env_remove("HTTP_PROXY");
    cmd.env_remove("HTTPS_PROXY");
    cmd.env_remove("ALL_PROXY");
    cmd.env_remove("CS_PROXY");
    cmd
}

fn run(home: &Path, args: &[&str]) -> Output {
    command(home, args).output().unwrap()
}

#[test]
fn command_failure_is_reported_once_and_kept_in_file_logs() {
    for json in [false, true] {
        for logging in ["default", "debug", "rust-log"] {
            let home = tempfile::tempdir().unwrap();
            let mut args = vec!["--color", "never"];
            if json {
                args.push("--json");
            }
            if logging == "debug" {
                args.push("--debug");
            }
            args.extend(["delete", "missing", "--yes"]);
            let mut cmd = command(home.path(), &args);
            cmd.env_remove("RUST_LOG");
            if logging == "rust-log" {
                cmd.env("RUST_LOG", "claude_switch=trace");
            }
            let output = output_with_timeout(&mut cmd, Duration::from_secs(15));
            assert!(!output.status.success());
            let stderr = String::from_utf8(output.stderr).unwrap();
            let error = if json {
                let report: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(report["ok"], false);
                assert!(!stderr.contains("not found"), "{stderr}");
                report["error"].as_str().unwrap().to_string()
            } else {
                assert!(output.stdout.is_empty());
                assert_eq!(stderr.matches("Error: ").count(), 1, "{stderr}");
                assert_eq!(stderr.matches("not found").count(), 1, "{stderr}");
                stderr
                    .lines()
                    .find_map(|line| line.strip_prefix("Error: "))
                    .unwrap()
                    .to_string()
            };
            assert!(!stderr.contains("command failed"), "{stderr}");
            let logs = fs::read_dir(home.path().join(".paper-claude-switch/logs"))
                .unwrap()
                .filter_map(|entry| fs::read_to_string(entry.unwrap().path()).ok())
                .collect::<String>();
            assert!(logs.contains("command failed"), "{logs}");
            assert!(logs.contains(&error), "{logs}");
        }
    }
}

fn run_with_env(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = command(home, args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().unwrap()
}

fn run_with_timeout(home: &Path, args: &[&str], timeout: Duration) -> Output {
    output_with_timeout(&mut command(home, args), timeout)
}

fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> Output {
    let mut child = cmd.spawn().unwrap();
    // Drain both pipes while waiting so verbose diagnostics cannot fill a pipe
    // and make an otherwise healthy process appear to hang.
    let read_pipe = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        })
    };
    let stdout = read_pipe(Box::new(child.stdout.take().unwrap()));
    let stderr = read_pipe(Box::new(child.stderr.take().unwrap()));
    let start = std::time::Instant::now();

    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Output {
                status,
                stdout: stdout.join().unwrap(),
                stderr: stderr.join().unwrap(),
            };
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            stdout.join().unwrap();
            stderr.join().unwrap();
            panic!("command timed out: {cmd:?}");
        }

        thread::sleep(Duration::from_millis(20));
    }
}

fn parse_stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn json_use_keeps_stdout_machine_readable() {
    let home = temp_home("json-use");
    write_live_login(&home, "alice@example.com", "U_alice", "tokAlice");
    write_claude_profile(&home, "alice", "alice@example.com", "U_alice", "tokAlice");
    write_claude_profile(&home, "bob", "bob@example.com", "U_bob", "tokBob");
    fs::create_dir_all(home.join(".paper-claude-switch")).unwrap();
    fs::write(home.join(".paper-claude-switch/current"), "alice").unwrap();

    let output = run(&home, &["--json", "use", "bob"]);
    assert!(output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({"ok": true, "alias": "bob", "action": "switched"})
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Switched to: bob"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_use_rejects_untracked_live_auth_without_prompting() {
    let home = temp_home("json-use-untracked");
    write_claude_profile(&home, "alice", "alice@example.com", "U_alice", "tokAlice");
    write_live_login(&home, "bob@example.com", "U_bob", "tokBob");

    let output = run(&home, &["--json", "use", "alice"]);
    assert!(!output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({
            "ok": false,
            "error": "the live Claude account (bob@example.com) is not saved; run `paper-claude-switch login` first"
        })
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("[y/N]"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_list_auto_track_keeps_stdout_machine_readable() {
    let home = temp_home("json-list");
    // Claude Code is logged in to an account that has no profile; carol is saved
    // but her credentials carry no access token.
    write_live_login(&home, "dave@example.com", "U_dave", "tokDave");
    write_claude_profile(&home, "carol", "carol@example.com", "U_carol", "unused");
    let mut credentials: Value = serde_json::from_str(
        &fs::read_to_string(home.join(".paper-claude-switch/profiles/carol/credentials.json"))
            .unwrap(),
    )
    .unwrap();
    credentials["claudeAiOauth"]
        .as_object_mut()
        .unwrap()
        .remove("accessToken");
    write_json(
        home.join(".paper-claude-switch/profiles/carol/credentials.json"),
        &credentials,
    );

    let output = run(&home, &["--json", "list"]);
    assert!(output.status.success());

    // The unsaved live account is neither announced on stdout nor saved.
    let stdout = parse_stdout_json(&output);
    assert_eq!(stdout["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(stdout["profiles"][0]["alias"], "carol");
    assert!(
        stdout["profiles"][0]["usage"]["error"]
            .as_str()
            .unwrap()
            .contains("Unauthorized")
    );
    assert!(!home.join(".paper-claude-switch/profiles/dave").exists());

    let _ = fs::remove_dir_all(home);
}

#[test]
fn zero_max_concurrent_is_sanitized() {
    let home = temp_home("zero-max-concurrent");
    write_claude_profile(&home, "dave", "dave@example.com", "U_dave", "tokDave");
    fs::create_dir_all(home.join(".paper-claude-switch")).unwrap();
    fs::write(home.join(".paper-claude-switch/current"), "dave").unwrap();
    fs::write(
        home.join(".paper-claude-switch/config.toml"),
        "[network]\nmax_concurrent = 0\n",
    )
    .unwrap();

    let output = run_with_timeout(&home, &["--json", "list"], Duration::from_secs(10));
    assert!(output.status.success());
    assert_eq!(parse_stdout_json(&output)["profiles"][0]["alias"], "dave");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("config.network.max_concurrent=0 is invalid; using 1 instead")
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn invalid_existing_config_fails_instead_of_using_defaults() {
    let home = temp_home("invalid-config");
    fs::create_dir_all(home.join(".paper-claude-switch")).unwrap();
    fs::write(
        home.join(".paper-claude-switch/config.toml"),
        "[network]\nmax_concurrent = \"many\"\n",
    )
    .unwrap();

    let output = run(&home, &["--json", "list"]);
    assert!(!output.status.success());
    let report = parse_stdout_json(&output);
    assert_eq!(report["ok"], false);
    assert!(report["error"].as_str().unwrap().contains("config.toml"));
    assert!(report["error"].as_str().unwrap().contains("parse"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn invalid_config_error_does_not_echo_proxy_credentials() {
    let home = temp_home("invalid-config-secret");
    fs::create_dir_all(home.join(".paper-claude-switch")).unwrap();
    fs::write(
        home.join(".paper-claude-switch/config.toml"),
        "[proxy]\nurl = \"http://user:SENTINEL_PASSWORD@example.com\n",
    )
    .unwrap();

    let output = run(&home, &["--json", "list"]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("failed to parse config file"));
    assert!(!stdout.contains("SENTINEL_PASSWORD"));
    assert!(!stderr.contains("SENTINEL_PASSWORD"));

    let _ = fs::remove_dir_all(home);
}

#[cfg(unix)]
#[test]
fn dangling_config_symlink_fails_instead_of_using_defaults() {
    use std::os::unix::fs::symlink;

    let home = temp_home("dangling-config-symlink");
    fs::create_dir_all(home.join(".paper-claude-switch")).unwrap();
    symlink(
        home.join("missing-config.toml"),
        home.join(".paper-claude-switch/config.toml"),
    )
    .unwrap();

    let output = run(&home, &["--json", "list"]);
    assert!(!output.status.success());
    let report = parse_stdout_json(&output);
    assert_eq!(report["ok"], false);
    assert!(report["error"].as_str().unwrap().contains("config.toml"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_delete_requires_explicit_yes_and_preserves_profile() {
    let home = temp_home("delete-json-confirm");
    write_claude_profile(&home, "gina", "gina@example.com", "U-gina", "tok-gina");

    let output = run(&home, &["--json", "delete", "gina"]);
    assert!(!output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({
            "ok": false,
            "error": "confirmation required; rerun with --yes to delete profile 'gina'"
        })
    );
    assert!(home.join(".paper-claude-switch/profiles/gina/credentials.json").exists());

    let _ = fs::remove_dir_all(home);
}

#[test]
fn non_interactive_delete_requires_explicit_yes_and_preserves_profile() {
    let home = temp_home("delete-non-interactive-confirm");
    write_claude_profile(&home, "gina", "gina@example.com", "U-gina", "tok-gina");

    let mut cmd = command(&home, &["delete", "gina"]);
    cmd.stdin(Stdio::null());
    let output = cmd.output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("confirmation required; rerun with --yes to delete profile 'gina'")
    );
    assert!(home.join(".paper-claude-switch/profiles/gina/credentials.json").exists());

    let _ = fs::remove_dir_all(home);
}

#[test]
fn delete_with_yes_archives_inactive_profile_for_recovery() {
    let home = temp_home("delete-yes");
    write_claude_profile(&home, "gina", "gina@example.com", "U-gina", "tok-gina");

    let output = run(&home, &["--json", "delete", "gina", "--yes"]);
    assert!(output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({"ok": true, "alias": "gina", "action": "deleted"})
    );
    assert!(!home.join(".paper-claude-switch/profiles/gina").exists());
    let deleted_dir = home.join(".paper-claude-switch/deleted-profiles");
    let archived: Vec<_> = fs::read_dir(&deleted_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(archived.len(), 1);
    assert!(
        archived[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("gina.backup-")
    );
    assert!(archived[0].join("credentials.json").exists());

    let _ = fs::remove_dir_all(home);
}

#[test]
fn delete_rejects_active_profile() {
    let home = temp_home("delete-active");
    write_claude_profile(&home, "gina", "gina@example.com", "U-gina", "tok-gina");
    write_live_login(&home, "gina@example.com", "U-gina", "tok-gina");
    fs::create_dir_all(home.join(".paper-claude-switch")).unwrap();
    fs::write(home.join(".paper-claude-switch/current"), "gina").unwrap();

    let output = run(&home, &["delete", "gina", "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot delete the active profile"));
    assert!(home.join(".paper-claude-switch/profiles/gina/credentials.json").exists());
    assert_eq!(
        fs::read_to_string(home.join(".paper-claude-switch/current")).unwrap(),
        "gina"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn automatic_use_without_profiles_explains_how_to_get_started() {
    let home = temp_home("use-no-profiles");

    let output = run(&home, &["--json", "use"]);
    assert!(!output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({
            "ok": false,
            "error": "no saved profiles; run `paper-claude-switch login` first"
        })
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_list_uses_per_account_cached_refresh_time() {
    let home = temp_home("json-list-cache-ts");
    write_claude_profile(&home, "ivy", "ivy@example.com", "U_ivy", "tokIvy");
    fs::create_dir_all(home.join(".paper-claude-switch")).unwrap();
    fs::write(home.join(".paper-claude-switch/current"), "ivy").unwrap();
    fs::write(
        home.join(".paper-claude-switch/config.toml"),
        "[cache]\nttl = 999999999\n",
    )
    .unwrap();

    write_cache_entry(&home, "ivy", 1_710_000_000, Some(42.0), Some(1_710_001_800));

    let output = run(&home, &["--json", "list"]);
    assert!(output.status.success());

    let stdout = parse_stdout_json(&output);
    assert_eq!(stdout["profiles"][0]["alias"], "ivy");
    assert_eq!(
        stdout["profiles"][0]["usage"]["primary"]["used_percent"],
        42.0
    );
    assert_eq!(
        stdout["profiles"][0]["usage"]["fetched_at"],
        "2024-03-09T16:00:00Z"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn non_interactive_stdin_does_not_save_new_account() {
    let home = temp_home("non-interactive-new");
    // Put a live Claude login with no matching profile
    write_live_login(&home, "notrack@example.com", "U_notrack", "tokNotrack");

    // Non-JSON, stdin closed: startup check should detect the new account but NOT save
    let output = command(&home, &["list"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should inform user about the new account (user_println goes to stdout in non-JSON mode)
    assert!(
        stdout.contains("Detected an unsaved Claude account"),
        "expected detection message in stdout, got: {stdout}"
    );
    // Should NOT have saved — no profiles directory should exist
    let profiles_dir = home.join(".paper-claude-switch/profiles");
    assert!(
        !profiles_dir.exists() || fs::read_dir(&profiles_dir).unwrap().count() == 0,
        "expected no profiles saved, but profiles dir has content"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn non_interactive_stdin_does_not_update_existing_profile() {
    let home = temp_home("non-interactive-update");
    // Create profile for alice
    write_claude_profile(&home, "alice", "alice@example.com", "U_alice", "tokOld");
    fs::create_dir_all(home.join(".paper-claude-switch")).unwrap();
    fs::write(home.join(".paper-claude-switch/current"), "alice").unwrap();

    // Claude Code rotated the live tokens for the same account
    write_live_login(&home, "alice@example.com", "U_alice", "tokRotated");

    // Run with stdin closed — listing must NOT copy the live tokens into the profile
    let output = command(&home, &["list"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());

    // Profile file should still have the original content (not updated)
    let profile_content: Value = serde_json::from_str(
        &fs::read_to_string(home.join(".paper-claude-switch/profiles/alice/credentials.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        profile_content["claudeAiOauth"]["refreshToken"], "refresh-tokOld",
        "profile refresh token should not have been updated"
    );
    assert_eq!(profile_content["claudeAiOauth"]["accessToken"], "tokOld");

    let _ = fs::remove_dir_all(home);
}

#[test]
fn list_progress_counts_only_stale_accounts() {
    let home = temp_home("list-progress-stale-only");
    write_claude_profile(&home, "fresh", "fresh@example.com", "U_fresh", "tokFresh");
    write_claude_profile(&home, "stale", "stale@example.com", "U_stale", "tokStale");
    fs::create_dir_all(home.join(".paper-claude-switch")).unwrap();
    fs::write(home.join(".paper-claude-switch/current"), "fresh").unwrap();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_cache_entry(&home, "fresh", now, Some(10.0), Some(now as i64 + 3600));

    let output = run_with_env(&home, &["list"], &[("CS_PROGRESS_FORCE", "1")]);
    assert!(output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Refreshing usage ["));
    assert!(stderr.contains("1/1"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn deleted_profile_can_be_listed_and_restored() {
    let home = temp_home("restore");
    for (alias, id) in [("alice", "U-a"), ("bob", "U-b")] {
        write_claude_profile(&home, alias, &format!("{alias}@example.com"), id, alias);
    }
    write_live_login(&home, "alice@example.com", "U-a", "alice");
    fs::write(home.join(".paper-claude-switch/current"), "alice").unwrap();

    let output = run(&home, &["delete", "bob", "--yes"]);
    assert!(output.status.success(), "{output:?}");
    assert!(!home.join(".paper-claude-switch/profiles/bob").exists());

    let output = run(&home, &["--json", "restore"]);
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({"deleted": ["bob"]})
    );

    let output = run(&home, &["--json", "restore", "bob"]);
    assert!(output.status.success(), "{output:?}");
    assert!(home.join(".paper-claude-switch/profiles/bob/credentials.json").exists());

    let output = run(&home, &["--json", "restore", "bob"]);
    assert!(!output.status.success(), "no archive is left to restore");

    let _ = fs::remove_dir_all(home);
}
