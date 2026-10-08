// `import-orca`: copy the Claude accounts the Orca app manages into saved
// profiles. Every spawned command points CLAUDE_CONFIG_DIR and
// PAPER_CLAUDE_SWITCH_HOME at a temp folder and reads Orca accounts from a temp
// folder through --from, so the real Claude login and the real Orca data are
// never read or written. Nothing here touches the network.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const T: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

fn write_json(path: impl AsRef<Path>, value: &Value) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn oauth(token: &str, expires_at: i64) -> Value {
    json!({
        "accessToken": token,
        "refreshToken": format!("rt-{token}"),
        "expiresAt": expires_at,
        "subscriptionType": "max"
    })
}

fn account(uuid: &str, email: &str) -> Value {
    json!({"accountUuid": uuid, "emailAddress": email})
}

/// Every file under `dir` with its bytes, keyed by the path relative to `dir`.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, found: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, found);
            } else {
                let key = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                found.insert(key, fs::read(&path).unwrap());
            }
        }
    }
    let mut found = BTreeMap::new();
    visit(dir, dir, &mut found);
    found
}

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("claude-orca-")
            .tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
            .unwrap();
        for dir in ["claude", "app", "home", "orca"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        Self { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn orca_dir(&self) -> PathBuf {
        self.path("orca")
    }

    /// One Orca account folder: `<orca>/<id>/auth/{.credentials.json,oauth-account.json}`.
    fn orca(&self, id: &str, token: &str, uuid: &str, email: &str, expires_at: i64, with_mcp: bool) {
        let auth = self.orca_dir().join(id).join("auth");
        let mut credentials = json!({"claudeAiOauth": oauth(token, expires_at)});
        if with_mcp {
            credentials["mcpOAuth"] = json!({"server": {"accessToken": "mcp-secret"}});
        }
        write_json(auth.join(".credentials.json"), &credentials);
        write_json(auth.join("oauth-account.json"), &account(uuid, email));
    }

    fn profile(&self, alias: &str, token: &str, uuid: &str, email: &str, expires_at: i64) {
        let dir = self.path("app/profiles").join(alias);
        write_json(dir.join("credentials.json"), &json!({"claudeAiOauth": oauth(token, expires_at)}));
        write_json(dir.join("account.json"), &account(uuid, email));
    }

    fn live(&self, token: &str, uuid: &str, email: &str) {
        write_json(
            self.path("claude/.credentials.json"),
            &json!({"claudeAiOauth": oauth(token, T), "mcpOAuth": {"m": 1}}),
        );
        write_json(
            self.path("claude/.claude.json"),
            &json!({"oauthAccount": account(uuid, email), "projects": {"p": 1}}),
        );
    }

    /// o1 (tokA, U1, a@x.com, with mcpOAuth) and o2 (tokB, U2, b@x.com).
    fn two_orca_accounts(&self) {
        self.orca("o1", "tokA", "U1", "a@x.com", T, true);
        self.orca("o2", "tokB", "U2", "b@x.com", T, false);
    }

    fn aliases(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.path("app/profiles"))
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
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

    /// `--json import-orca --from <orca dir> [extra...]`, parsed from stdout.
    fn import_json(&self, extra: &[&str]) -> (Output, Value) {
        let from = self.orca_dir();
        let mut args = vec!["--json", "import-orca", "--from", from.to_str().unwrap()];
        args.extend_from_slice(extra);
        let output = self.run(&args);
        let value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        (output, value)
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

/// (alias, action, source) of every entry in `accounts`, sorted.
fn actions(value: &Value) -> Vec<(String, String, String)> {
    let mut found: Vec<(String, String, String)> = value["accounts"]
        .as_array()
        .unwrap_or_else(|| panic!("no accounts array in {value}"))
        .iter()
        .map(|a| {
            let text = |key: &str| a[key].as_str().unwrap_or_default().to_string();
            (text("alias"), text("action"), text("source"))
        })
        .collect();
    found.sort();
    found
}

fn entry(alias: &str, action: &str, source: &str) -> (String, String, String) {
    (alias.to_string(), action.to_string(), source.to_string())
}

#[test]
fn import_creates_a_profile_per_orca_account_without_mcp_oauth() {
    let f = Fixture::new();
    f.two_orca_accounts();
    let (output, value) = f.import_json(&[]);
    success(&output);
    assert_eq!(
        actions(&value),
        vec![entry("a", "created", "o1"), entry("b", "created", "o2")],
        "{}",
        describe(&output)
    );
    assert_eq!(value["dry_run"], false, "{}", describe(&output));
    assert_eq!(f.aliases(), vec!["a".to_string(), "b".to_string()]);
    assert_eq!(
        read_json(f.path("app/profiles/a/credentials.json")),
        json!({"claudeAiOauth": oauth("tokA", T)})
    );
    assert_eq!(read_json(f.path("app/profiles/a/account.json")), account("U1", "a@x.com"));
    assert_eq!(
        read_json(f.path("app/profiles/b/credentials.json")),
        json!({"claudeAiOauth": oauth("tokB", T)})
    );
    assert_eq!(read_json(f.path("app/profiles/b/account.json")), account("U2", "b@x.com"));
    assert!(!f.path("app/current").exists(), "current marker must not be created");
}

#[test]
fn import_updates_a_saved_profile_when_orca_refreshed_it_more_recently() {
    let f = Fixture::new();
    f.profile("work", "tokOld", "U2", "b@x.com", T);
    f.orca("o2", "tokB", "U2", "b@x.com", T + HOUR, false);
    let (output, value) = f.import_json(&[]);
    success(&output);
    assert_eq!(actions(&value), vec![entry("work", "updated", "o2")], "{}", describe(&output));
    assert_eq!(f.aliases(), vec!["work".to_string()]);
    assert_eq!(
        read_json(f.path("app/profiles/work/credentials.json")),
        json!({"claudeAiOauth": oauth("tokB", T + HOUR)})
    );
    assert_eq!(read_json(f.path("app/profiles/work/account.json")), account("U2", "b@x.com"));
}

#[test]
fn import_keeps_a_saved_profile_that_is_at_least_as_fresh_as_orca() {
    let f = Fixture::new();
    f.profile("work", "tokNew", "U2", "b@x.com", T + HOUR);
    f.orca("o2", "tokB", "U2", "b@x.com", T, false);
    let before = snapshot(&f.path("app/profiles"));
    let (output, value) = f.import_json(&[]);
    success(&output);
    assert_eq!(actions(&value), vec![entry("work", "kept", "o2")], "{}", describe(&output));
    assert_eq!(snapshot(&f.path("app/profiles")), before);
}

#[test]
fn import_skips_unreadable_accounts_and_still_imports_the_rest() {
    let f = Fixture::new();
    f.orca("o1", "tokA", "U1", "a@x.com", T, false);
    // o2: no oauth-account.json.
    f.orca("o2", "tokB", "U2", "b@x.com", T, false);
    fs::remove_file(f.orca_dir().join("o2/auth/oauth-account.json")).unwrap();
    // o3: credentials are not JSON.
    f.orca("o3", "tokC", "U3", "c@x.com", T, false);
    fs::write(f.orca_dir().join("o3/auth/.credentials.json"), "{ not json").unwrap();
    // o4: credentials without claudeAiOauth.
    f.orca("o4", "tokD", "U4", "d@x.com", T, false);
    write_json(f.orca_dir().join("o4/auth/.credentials.json"), &json!({"mcpOAuth": {"m": 1}}));

    let (output, value) = f.import_json(&[]);
    success(&output);
    assert_eq!(actions(&value), vec![entry("a", "created", "o1")], "{}", describe(&output));
    assert_eq!(f.aliases(), vec!["a".to_string()]);
    let mut skipped: Vec<(String, bool)> = value["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("no skipped array in {value}"))
        .iter()
        .map(|s| {
            (
                s["source"].as_str().unwrap_or_default().to_string(),
                !s["reason"].as_str().unwrap_or_default().is_empty(),
            )
        })
        .collect();
    skipped.sort();
    assert_eq!(
        skipped,
        vec![("o2".to_string(), true), ("o3".to_string(), true), ("o4".to_string(), true)],
        "{}",
        describe(&output)
    );
}

#[test]
fn import_dry_run_reports_the_actions_and_writes_nothing() {
    let f = Fixture::new();
    f.two_orca_accounts();
    let (output, value) = f.import_json(&["--dry-run"]);
    success(&output);
    assert_eq!(
        actions(&value),
        vec![entry("a", "created", "o1"), entry("b", "created", "o2")],
        "{}",
        describe(&output)
    );
    assert_eq!(value["dry_run"], true, "{}", describe(&output));
    assert!(f.aliases().is_empty(), "dry run wrote profiles: {:?}", f.aliases());
    assert!(!f.path("app/current").exists());
}

#[test]
fn import_leaves_orca_and_the_live_claude_login_untouched() {
    let f = Fixture::new();
    f.two_orca_accounts();
    f.live("tokLive", "U9", "live@x.com");
    let orca_before = snapshot(&f.orca_dir());
    let live_before = snapshot(&f.path("claude"));
    assert!(!orca_before.is_empty() && !live_before.is_empty());

    let from = f.orca_dir();
    let output = f.run(&["import-orca", "--from", from.to_str().unwrap()]);
    success(&output);
    assert_eq!(snapshot(&f.orca_dir()), orca_before);
    assert_eq!(snapshot(&f.path("claude")), live_before);
    assert!(f.path("app/profiles/a/credentials.json").exists(), "{}", describe(&output));
    assert!(!f.path("app/current").exists());
}

#[test]
fn import_fails_when_the_orca_dir_is_missing() {
    let f = Fixture::new();
    let missing = f.path("no-such-orca-dir");
    let output = f.run(&["import-orca", "--from", missing.to_str().unwrap()]);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(missing.to_str().unwrap()),
        "{}",
        describe(&output)
    );
}

#[test]
fn import_gives_a_clashing_email_local_part_a_numbered_alias() {
    let f = Fixture::new();
    f.orca("o1", "tokA", "U1", "a@x.com", T, false);
    f.orca("o2", "tokB", "U2", "a@y.com", T, false);
    let (output, value) = f.import_json(&[]);
    success(&output);
    assert_eq!(f.aliases(), vec!["a".to_string(), "a_2".to_string()], "{}", describe(&output));
    let aliases: Vec<String> = actions(&value).into_iter().map(|(alias, _, _)| alias).collect();
    assert_eq!(aliases, vec!["a".to_string(), "a_2".to_string()], "{}", describe(&output));
}

#[test]
fn import_human_output_lists_each_action_and_the_orca_hint() {
    let f = Fixture::new();
    f.two_orca_accounts();
    let from = f.orca_dir();
    let output = f.run(&["import-orca", "--from", from.to_str().unwrap()]);
    success(&output);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("created a"), "{}", describe(&output));
    assert!(text.contains("Remove these accounts from Orca"), "{}", describe(&output));
}
