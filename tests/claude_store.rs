use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_switch::claude_store::{
    ClaudePaths, LockOptions, SaveAction, SwitchOutcome, save_current, switch_to, write_live,
};
use serde_json::{Value, json};
use tempfile::TempDir;

// Fixtures stay in the worktree and never consult the real home or process environment.
fn temp_dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap()
}

fn lock_options() -> LockOptions {
    LockOptions {
        timeout: Duration::from_secs(9),
        credentials_stale: Duration::from_secs(60),
        config_stale: Duration::from_secs(10),
    }
}

fn oauth(label: &str) -> Value {
    json!({
        "accessToken": format!("fake-access-{label}"),
        "refreshToken": format!("fake-refresh-{label}"),
        "expiresAt": 1_800_000_000_000_u64,
        "refreshTokenExpiresAt": 1_900_000_000_000_u64,
        "scopes": ["user:inference", "user:profile"],
        "subscriptionType": "pro",
        "rateLimitTier": "default_claude_ai"
    })
}

fn account(uuid: &str, email: &str) -> Value {
    json!({
        "accountUuid": uuid,
        "emailAddress": email,
        "organizationUuid": format!("org-{uuid}"),
        "displayName": format!("Test {uuid}")
    })
}

fn machine_oauth() -> Value {
    json!({"local-server": {"accessToken": "fake-machine-token"}})
}

fn config_with(account: &Value) -> Value {
    json!({
        "oauthAccount": account,
        "projects": {"/test/project": {"allowedTools": ["Read"]}},
        "numStartups": 5,
        "mcpServers": {"local": {"command": "test-server"}}
    })
}

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

struct Fixture {
    root: TempDir,
    paths: ClaudePaths,
    app_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = temp_dir();
        let home = root.path().join("home");
        let config_home = home.join(".claude");
        fs::create_dir_all(&config_home).unwrap();
        let paths = ClaudePaths {
            credentials: config_home.join(".credentials.json"),
            global_config: home.join(".claude.json"),
            config_home,
        };
        let app_home = root.path().join("app");
        fs::create_dir_all(&app_home).unwrap();
        Self { root, paths, app_home }
    }

    fn seed_live(&self, oauth: &Value, account: &Value) {
        write_json(
            &self.paths.credentials,
            &json!({"claudeAiOauth": oauth, "mcpOAuth": machine_oauth()}),
        );
        write_json(&self.paths.global_config, &config_with(account));
    }

    fn seed_profile(&self, alias: &str, oauth: &Value, account: &Value) {
        let dir = self.app_home.join("profiles").join(alias);
        write_json(&dir.join("credentials.json"), &json!({"claudeAiOauth": oauth}));
        write_json(&dir.join("account.json"), account);
    }

    fn mark_current(&self, alias: &str) {
        fs::write(self.app_home.join("current"), alias).unwrap();
    }

    fn current(&self) -> String {
        fs::read_to_string(self.app_home.join("current")).unwrap().trim().to_owned()
    }

    fn profile_names(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(self.app_home.join("profiles")).unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn credentials_lock(&self) -> PathBuf {
        let mut path = self.paths.config_home.as_os_str().to_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }
}

// Include directories as well as file bytes to detect extra profiles or abandoned locks.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, dir: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                entries.insert(relative, None);
                visit(root, &path, entries);
            } else {
                entries.insert(relative, Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn assert_no_lock_dirs(root: &Path) {
    for (path, contents) in snapshot(root) {
        if contents.is_none() {
            assert!(!path.file_name().unwrap().to_string_lossy().ends_with(".lock"),
                "lock directory left behind: {}", path.display());
        }
    }
}

fn assert_saved(f: &Fixture, alias: &str, oauth: &Value, account: &Value) {
    let dir = f.app_home.join("profiles").join(alias);
    assert_eq!(read_json(&dir.join("credentials.json")), json!({"claudeAiOauth": oauth}));
    assert_eq!(read_json(&dir.join("account.json")), *account);
}

fn switching_fixture(live_uuid: &str, live_oauth: &Value) -> Fixture {
    let f = Fixture::new();
    f.seed_profile("a", &oauth("a"), &account("U1", "a@x.com"));
    f.seed_profile("b", &oauth("b"), &account("U2", "b@x.com"));
    f.mark_current("a");
    f.seed_live(live_oauth, &account(live_uuid, "a@x.com"));
    f
}

#[test]
fn resolve_default_home_paths() {
    let root = temp_dir();
    let home = root.path().join("home");
    let paths = ClaudePaths::resolve(None, &home);
    assert_eq!(paths.config_home, home.join(".claude"));
    assert_eq!(paths.credentials, home.join(".claude/.credentials.json"));
    assert_eq!(paths.global_config, home.join(".claude.json"));
}

#[test]
fn resolve_config_dir_override_paths() {
    let root = temp_dir();
    let home = root.path().join("home");
    let dir = root.path().join("override");
    let paths = ClaudePaths::resolve(Some(&dir), &home);
    assert_eq!(paths.config_home, dir);
    assert_eq!(paths.credentials, dir.join(".credentials.json"));
    assert_eq!(paths.global_config, dir.join(".claude.json"));
}

#[test]
fn resolve_prefers_existing_legacy_global_config() {
    let root = temp_dir();
    let home = root.path().join("home");
    let dir = root.path().join("override");
    write_json(&dir.join(".config.json"), &json!({}));
    let paths = ClaudePaths::resolve(Some(&dir), &home);
    assert_eq!(paths.config_home, dir);
    assert_eq!(paths.credentials, dir.join(".credentials.json"));
    assert_eq!(paths.global_config, dir.join(".config.json"));
}

#[test]
fn write_live_preserves_machine_keys_and_removes_owned_locks() {
    let f = Fixture::new();
    f.seed_live(&oauth("a"), &account("U1", "a@x.com"));
    let b = oauth("b");
    let y = account("U2", "b@x.com");
    write_live(&f.paths, &b, &y, &lock_options()).unwrap();
    assert_eq!(read_json(&f.paths.credentials), json!({"claudeAiOauth": b, "mcpOAuth": machine_oauth()}));
    assert_eq!(read_json(&f.paths.global_config), config_with(&y));
    assert_no_lock_dirs(f.root.path());
}

#[test]
fn write_live_creates_missing_credentials_file() {
    let f = Fixture::new();
    let b = oauth("b");
    let y = account("U2", "b@x.com");
    write_json(&f.paths.global_config, &config_with(&y));
    assert!(!f.paths.credentials.exists());
    write_live(&f.paths, &b, &y, &lock_options()).unwrap();
    assert_eq!(read_json(&f.paths.credentials), json!({"claudeAiOauth": b}));
}

#[test]
fn write_live_fresh_credentials_lock_times_out_without_mutation() {
    let f = Fixture::new();
    f.seed_live(&oauth("a"), &account("U1", "a@x.com"));
    let existing_lock = f.credentials_lock();
    fs::create_dir(&existing_lock).unwrap();
    let before = snapshot(f.root.path());
    let mut lock = lock_options();
    lock.timeout = Duration::from_secs(1);
    let err = write_live(&f.paths, &oauth("b"), &account("U2", "b@x.com"), &lock).unwrap_err();
    assert!(format!("{err:#}").contains("lock"), "{err:#}");
    assert_eq!(snapshot(f.root.path()), before);
    assert!(existing_lock.is_dir());
}

#[test]
fn write_live_takes_over_stale_credentials_lock() {
    let f = Fixture::new();
    f.seed_live(&oauth("a"), &account("U1", "a@x.com"));
    let existing_lock = f.credentials_lock();
    fs::create_dir(&existing_lock).unwrap();
    let mut lock = lock_options();
    lock.credentials_stale = Duration::ZERO;
    let b = oauth("b");
    let y = account("U2", "b@x.com");
    write_live(&f.paths, &b, &y, &lock).unwrap();
    assert_eq!(read_json(&f.paths.credentials), json!({"claudeAiOauth": b, "mcpOAuth": machine_oauth()}));
    assert_eq!(read_json(&f.paths.global_config), config_with(&y));
    assert!(!existing_lock.exists());
    assert_no_lock_dirs(f.root.path());
}

#[test]
fn save_current_creates_account_scoped_profile_and_marker() {
    let f = Fixture::new();
    let a = oauth("a");
    let x = account("U1", "a@x.com");
    f.seed_live(&a, &x);
    let action = save_current(&f.paths, &f.app_home, None, &lock_options()).unwrap();
    let SaveAction::Created(alias) = action else { panic!("expected Created, got {action:?}"); };
    assert!(!alias.is_empty());
    assert_saved(&f, &alias, &a, &x);
    assert_eq!(f.current(), alias);
    assert_eq!(f.profile_names(), vec![alias]);
}

#[test]
fn save_current_updates_same_uuid_after_refresh_token_rotation() {
    let f = Fixture::new();
    let a = oauth("a");
    let x = account("U1", "a@x.com");
    f.seed_live(&a, &x);
    let action = save_current(&f.paths, &f.app_home, None, &lock_options()).unwrap();
    let SaveAction::Created(alias) = action else { panic!("expected Created, got {action:?}"); };
    let mut rotated = a;
    rotated["refreshToken"] = json!("fake-rotated-refresh-a");
    f.seed_live(&rotated, &x);
    assert_eq!(save_current(&f.paths, &f.app_home, None, &lock_options()).unwrap(), SaveAction::Updated(alias.clone()));
    assert_eq!(f.profile_names(), vec![alias.clone()]);
    assert_saved(&f, &alias, &rotated, &x);
    assert_eq!(f.current(), alias);
}

#[test]
fn save_current_creates_explicit_alias_for_second_uuid() {
    let f = Fixture::new();
    let a = oauth("a");
    let x = account("U1", "a@x.com");
    f.seed_live(&a, &x);
    let action = save_current(&f.paths, &f.app_home, None, &lock_options()).unwrap();
    let SaveAction::Created(alias) = action else { panic!("expected Created, got {action:?}"); };
    let b = oauth("b");
    let y = account("U2", "b@x.com");
    f.seed_live(&b, &y);
    assert_eq!(save_current(&f.paths, &f.app_home, Some("work"), &lock_options()).unwrap(), SaveAction::Created("work".into()));
    let mut expected = vec![alias.clone(), "work".to_owned()];
    expected.sort();
    assert_eq!(f.profile_names(), expected);
    assert_saved(&f, &alias, &a, &x);
    assert_saved(&f, "work", &b, &y);
    assert_eq!(f.current(), "work");
}

#[test]
fn switch_to_captures_rotated_live_credentials_and_preserves_machine_state() {
    let mut rotated = oauth("a");
    rotated["refreshToken"] = json!("fake-rotated-refresh-a");
    let f = switching_fixture("U1", &rotated);
    let b_dir = f.app_home.join("profiles/b");
    let b_before = snapshot(&b_dir);
    assert_eq!(switch_to(&f.paths, &f.app_home, "b", &lock_options()).unwrap(),
        SwitchOutcome::Switched { from: Some("a".into()), to: "b".into() });
    assert_saved(&f, "a", &rotated, &account("U1", "a@x.com"));
    assert_eq!(read_json(&f.paths.credentials), json!({"claudeAiOauth": oauth("b"), "mcpOAuth": machine_oauth()}));
    assert_eq!(read_json(&f.paths.global_config), config_with(&account("U2", "b@x.com")));
    assert_eq!(snapshot(&b_dir), b_before);
    assert_eq!(f.current(), "b");
    assert_no_lock_dirs(f.root.path());
}

#[test]
fn switch_to_rejects_unsaved_live_uuid_without_mutation() {
    let f = switching_fixture("U3", &oauth("unknown"));
    let before = snapshot(f.root.path());
    let err = switch_to(&f.paths, &f.app_home, "b", &lock_options()).unwrap_err();
    assert!(format!("{err:#}").contains("not saved"), "{err:#}");
    assert_eq!(snapshot(f.root.path()), before);
}

#[test]
fn switch_to_active_account_is_noop() {
    let f = switching_fixture("U1", &oauth("a"));
    let before = snapshot(f.root.path());
    assert_eq!(switch_to(&f.paths, &f.app_home, "a", &lock_options()).unwrap(), SwitchOutcome::AlreadyActive);
    assert_eq!(snapshot(f.root.path()), before);
}

#[test]
fn switch_to_missing_alias_returns_named_error_without_mutation() {
    let f = switching_fixture("U1", &oauth("a"));
    let before = snapshot(f.root.path());
    let err = switch_to(&f.paths, &f.app_home, "nope", &lock_options()).unwrap_err();
    assert!(format!("{err:#}").contains("nope"), "{err:#}");
    assert_eq!(snapshot(f.root.path()), before);
}

#[test]
fn write_live_keeps_global_config_key_order() {
    let f = Fixture::new();
    write_json(&f.paths.credentials, &json!({"claudeAiOauth": oauth("a")}));
    fs::write(&f.paths.global_config,
        r#"{"zeta": 1, "oauthAccount": {"accountUuid": "U1"}, "alpha": 2}"#).unwrap();
    write_live(&f.paths, &oauth("b"), &account("U2", "b@x.com"), &lock_options()).unwrap();
    let text = fs::read_to_string(&f.paths.global_config).unwrap();
    let (zeta, oauth_account, alpha) =
        (text.find("\"zeta\"").unwrap(), text.find("\"oauthAccount\"").unwrap(), text.find("\"alpha\"").unwrap());
    assert!(zeta < oauth_account && oauth_account < alpha, "{text}");
}
