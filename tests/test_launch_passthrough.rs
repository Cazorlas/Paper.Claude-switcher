// `launch` argument handling: what is an alias and what goes to Claude Code.
// CS_CLAUDE_BIN points at a stand-in script that records its argv, so the real
// `claude` and the real Claude login are never touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_home(name: &str) -> PathBuf {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("claude-switch-{name}-{ts}-{id}"));
    fs::create_dir_all(&path).unwrap();
    path
}

/// A `claude` stand-in that writes one argument per line to `$CS_FAKE_LOG`.
fn install_fake_claude(home: &Path) -> (PathBuf, PathBuf) {
    let log = home.join("argv.log");
    #[cfg(unix)]
    let script = {
        use std::os::unix::fs::PermissionsExt;
        let script = home.join("fake_claude.sh");
        fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CS_FAKE_LOG\"\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    };
    #[cfg(windows)]
    let script = {
        let script = home.join("fake_claude.cmd");
        fs::write(
            &script,
            "@echo off\r\n(for %%a in (%*) do echo %%~a)> \"%CS_FAKE_LOG%\"\r\n",
        )
        .unwrap();
        script
    };
    (script, log)
}

fn write_claude_profile(home: &Path, alias: &str, email: &str, uuid: &str) {
    let dir = home.join(".paper-claude-switch/profiles").join(alias);
    fs::create_dir_all(&dir).unwrap();
    let oauth = json!({
        "accessToken": format!("tok-{alias}"),
        "refreshToken": format!("rt-{alias}"),
        "expiresAt": 4_102_444_800_000_i64,
        "subscriptionType": "pro"
    });
    fs::write(
        dir.join("credentials.json"),
        serde_json::to_vec_pretty(&json!({"claudeAiOauth": oauth})).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join("account.json"),
        serde_json::to_vec_pretty(&json!({"accountUuid": uuid, "emailAddress": email})).unwrap(),
    )
    .unwrap();
}

fn run(home: &Path, fake: &(PathBuf, PathBuf), args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_paper-claude-switch"));
    cmd.args(args)
        .stdin(Stdio::null())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("PAPER_CLAUDE_SWITCH_HOME", home.join(".paper-claude-switch"))
        .env("CS_CLAUDE_BIN", &fake.0)
        .env("CS_FAKE_LOG", &fake.1)
        .env("NO_COLOR", "1")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("CS_PROXY");
    cmd.output().unwrap()
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn recorded_argv(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn launch_dash_dash_exec_json_is_not_an_alias_named_exec() {
    let home = temp_home("dash-dash-exec");
    let fake = install_fake_claude(&home);

    let output = run(&home, &fake, &["launch", "--", "mcp", "--json", "review this"]);
    let combined = combined(&output);
    assert!(!output.status.success(), "auto-select with no profiles must fail: {combined}");
    assert!(
        combined.contains("no saved profiles"),
        "launch -- exec must auto-select, not look up alias exec: {combined}"
    );
    assert!(!combined.contains("profile 'exec' not found"), "{combined}");
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_exec_without_separator_is_not_an_alias() {
    let home = temp_home("exec-not-alias");
    let fake = install_fake_claude(&home);

    let output = run(&home, &fake, &["launch", "mcp", "--json", "review this"]);
    let combined = combined(&output);
    assert!(!output.status.success(), "auto-select with no profiles must fail: {combined}");
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
    let fake = install_fake_claude(&home);
    write_claude_profile(&home, "work", "work@example.com", "U-work");

    let output = run(&home, &fake, &["launch", "work", "mcp", "--", "--json", "hi"]);
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(recorded_argv(&fake.1), ["mcp", "--json", "hi"]);
    let _ = fs::remove_dir_all(home);
}
