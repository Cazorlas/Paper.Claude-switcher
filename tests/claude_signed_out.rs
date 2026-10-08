// A Claude Code login that was signed out keeps `oauthAccount` and a
// `claudeAiOauth` object whose tokens are empty. Such a login must never be
// saved over a profile: it would replace a working login with nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_switch::claude_store::{
    ClaudePaths, LockOptions, ProfileImporter, SwitchOutcome, account_from_parts, read_live,
    save_current, switch_to,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn lock() -> LockOptions {
    LockOptions {
        timeout: Duration::from_secs(9),
        credentials_stale: Duration::from_secs(60),
        config_stale: Duration::from_secs(10),
    }
}

fn oauth(token: &str) -> Value {
    json!({
        "accessToken": token,
        "refreshToken": format!("rt-{token}"),
        "expiresAt": 1_800_000_000_000_u64,
        "subscriptionType": "max"
    })
}

fn signed_out() -> Value {
    json!({
        "accessToken": "",
        "refreshToken": "",
        "expiresAt": 0,
        "subscriptionType": "pro"
    })
}

fn account(uuid: &str, email: &str) -> Value {
    json!({"accountUuid": uuid, "emailAddress": email})
}

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

struct Fixture {
    _root: TempDir,
    paths: ClaudePaths,
    app: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target")).unwrap();
        let home = root.path().join("home");
        let paths = ClaudePaths::resolve(None, &home);
        let app = root.path().join("app");
        fs::create_dir_all(&app).unwrap();
        Self { _root: root, paths, app }
    }

    fn live(&self, oauth: Value, account: Value) {
        write_json(&self.paths.credentials, &json!({"claudeAiOauth": oauth}));
        write_json(&self.paths.global_config, &json!({"oauthAccount": account}));
    }

    fn profile(&self, alias: &str, oauth: Value, account: Value) {
        let dir = self.app.join("profiles").join(alias);
        write_json(&dir.join("credentials.json"), &json!({"claudeAiOauth": oauth}));
        write_json(&dir.join("account.json"), &account);
    }

    fn saved_token(&self, alias: &str) -> Value {
        read_json(&self.app.join("profiles").join(alias).join("credentials.json"))["claudeAiOauth"]
            ["accessToken"]
            .clone()
    }
}

#[test]
fn a_signed_out_live_login_is_not_a_live_account() {
    let f = Fixture::new();
    f.live(signed_out(), account("U1", "a@x.com"));
    assert!(read_live(&f.paths).unwrap().is_none());
}

#[test]
fn login_refuses_to_save_a_signed_out_login_over_a_working_profile() {
    let f = Fixture::new();
    f.profile("a", oauth("tokA"), account("U1", "a@x.com"));
    f.live(signed_out(), account("U1", "a@x.com"));
    assert!(save_current(&f.paths, &f.app, None, &lock()).is_err());
    assert_eq!(f.saved_token("a"), "tokA");
}

#[test]
fn switching_away_from_a_signed_out_login_keeps_the_saved_profile() {
    let f = Fixture::new();
    f.profile("a", oauth("tokA"), account("U1", "a@x.com"));
    f.profile("b", oauth("tokB"), account("U2", "b@x.com"));
    fs::write(f.app.join("current"), "a").unwrap();
    f.live(signed_out(), account("U1", "a@x.com"));

    let outcome = switch_to(&f.paths, &f.app, "b", &lock()).unwrap();

    assert_eq!(outcome, SwitchOutcome::Switched { from: None, to: "b".into() });
    assert_eq!(f.saved_token("a"), "tokA");
    let live = read_json(&f.paths.credentials);
    assert_eq!(live["claudeAiOauth"]["accessToken"], "tokB");
}

#[test]
fn a_signed_out_login_from_another_app_is_rejected_for_import() {
    let f = Fixture::new();
    f.profile("a", oauth("tokA"), account("U1", "a@x.com"));
    let parts = account_from_parts(&json!({"claudeAiOauth": signed_out()}), &account("U1", "a@x.com"));
    let error = format!("{:#}", parts.err().expect("signed-out login must be rejected"));
    assert!(error.contains("signed out"), "{error}");
    // And the importer never sees it, so the saved profile is untouched.
    let _importer = ProfileImporter::new(&f.app, false).unwrap();
    assert_eq!(f.saved_token("a"), "tokA");
}
