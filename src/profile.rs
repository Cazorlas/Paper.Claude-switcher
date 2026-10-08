use std::fs::{File, OpenOptions};
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};

use crate::auth::{app_home, atomic_write_private, current_file, profiles_dir};
use crate::error::CsError;
use crate::output::user_println;

const MAX_ALIAS_LEN: usize = 64;

/// The saved Claude credentials of a profile (`credentials.json`).
pub fn profile_auth_path(alias: &str) -> Result<PathBuf> {
    Ok(profiles_dir()?.join(alias).join("credentials.json"))
}

pub fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty() {
        anyhow::bail!("alias cannot be empty");
    }
    if alias == "." || alias == ".." {
        anyhow::bail!("alias cannot be '.' or '..'");
    }
    if alias.len() > MAX_ALIAS_LEN {
        anyhow::bail!("alias must be at most {MAX_ALIAS_LEN} characters");
    }
    if !alias
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        anyhow::bail!("alias may only contain ASCII letters, digits, '_', '-', '.'");
    }
    Ok(())
}

pub fn list_profiles() -> Result<Vec<String>> {
    let dir = profiles_dir()?;
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading profiles directory {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    Ok(names)
}

pub fn read_current() -> String {
    current_file()
        .and_then(|p| std::fs::read_to_string(p).map_err(Into::into))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("creating directory {}", path.display()))?;
    #[cfg(windows)]
    crate::auth::harden_windows_private_directory(path)
        .with_context(|| format!("securing directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

fn deleted_profiles_dir() -> Result<PathBuf> {
    Ok(app_home()?.join("deleted-profiles"))
}

fn auth_lock_path() -> Result<PathBuf> {
    Ok(app_home()?.join("auth.lock"))
}

fn launch_lock_path() -> Result<PathBuf> {
    Ok(app_home()?.join("launch.lock"))
}

/// Maximum time to wait for an auth-related lock. A timeout is reported rather
/// than replacing the inode because an OS lock is the only reliable liveness signal.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn lock_live_auth() -> Result<File> {
    let path = auth_lock_path()?;
    acquire_file_lock(&path, LOCK_WAIT_TIMEOUT, "auth")
}

/// Serialize the short launch window so a switch never lands in the middle of it.
pub fn lock_launch_session() -> Result<File> {
    let path = launch_lock_path()?;
    acquire_file_lock(&path, LOCK_WAIT_TIMEOUT, "launch session")
}

struct AuthTransaction {
    _launch: File,
    _auth: File,
}

fn lock_auth_transaction() -> Result<AuthTransaction> {
    // Every writer uses this order: launch lock first, then the auth lock.
    let launch = lock_launch_session()?;
    let auth = lock_live_auth()?;
    Ok(AuthTransaction {
        _launch: launch,
        _auth: auth,
    })
}

fn acquire_file_lock(path: &Path, timeout: Duration, label: &str) -> Result<File> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    let file = open_lock_file(path)?;
    let deadline = Instant::now() + timeout;
    let waited = Instant::now();
    let mut waited_ms: u64 = 0;
    loop {
        match FileExt::try_lock(&file) {
            Ok(()) => {
                write_lock_holder(&file);
                if waited_ms > 0 {
                    tracing::info!(lock = label, waited_ms, "acquired contested file lock");
                }
                return Ok(file);
            }
            Err(TryLockError::WouldBlock) => {
                #[cfg(test)]
                notify_test_lock_attempt(label);
                if Instant::now() >= deadline {
                    let holder =
                        read_lock_holder(path).unwrap_or_else(|| "unknown holder".to_string());
                    anyhow::bail!(
                        "{label} lock {} remained held for {:.3}s by {holder}; refusing to replace the live lock file",
                        path.display(),
                        timeout.as_secs_f64(),
                    );
                }
                std::thread::sleep(LOCK_POLL_INTERVAL);
                waited_ms = waited.elapsed().as_millis() as u64;
            }
            Err(TryLockError::Error(e)) => {
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_LOCK_ATTEMPT_NOTIFIER:
        std::cell::RefCell<Option<(String, std::sync::mpsc::Sender<()>)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn notify_on_test_lock_attempt(label: &str, sender: std::sync::mpsc::Sender<()>) {
    TEST_LOCK_ATTEMPT_NOTIFIER.with(|notifier| {
        *notifier.borrow_mut() = Some((label.to_string(), sender));
    });
}

#[cfg(test)]
fn notify_test_lock_attempt(label: &str) {
    TEST_LOCK_ATTEMPT_NOTIFIER.with(|notifier| {
        let should_notify = notifier
            .borrow()
            .as_ref()
            .is_some_and(|(target, _)| target == label);
        if should_notify && let Some((_, sender)) = notifier.borrow_mut().take() {
            let _ = sender.send(());
        }
    });
}

/// Open a stable lock inode. Permission/ownership errors are reported rather
/// than recovered by unlinking because another process may still hold it.
fn open_lock_file(path: &Path) -> Result<File> {
    try_open_lock_file(path).with_context(|| {
        format!(
            "opening auth lock {}; check the file and parent directory ownership",
            path.display()
        )
    })
}

fn try_open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

/// Best-effort: write `pid epoch_secs` to the lock file for diagnostics.
/// Failure is non-fatal — the OS-level flock is the source of truth.
fn write_lock_holder(file: &File) {
    use std::io::Seek;
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("{pid} {ts}\n");
    let _ = file.set_len(0);
    let mut f = file;
    let _ = f.seek(std::io::SeekFrom::Start(0));
    let _ = f.write_all(line.as_bytes());
}

fn read_lock_holder(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_current(alias: &str) -> Result<()> {
    let path = current_file()?;
    atomic_write_private(&path, alias.as_bytes())
        .with_context(|| format!("writing current profile marker {}", path.display()))?;
    Ok(())
}

/// The profile of the live Claude account; the `current` marker is repaired
/// to name it.
pub fn sync_current_from_live() -> Option<String> {
    crate::claude_usage::current_active()
}

/// Make `alias` the live Claude account. The app lock is held so two
/// processes (`auto`, the TUI, a command) never swap or refresh at once.
pub fn switch_profile(alias: &str) -> Result<crate::claude_store::SwitchOutcome> {
    validate_alias(alias)?;
    let paths = crate::claude_usage::paths()?;
    let _lock = lock_live_auth()?;
    crate::claude_store::switch_to(
        &paths,
        &app_home()?,
        alias,
        &crate::claude_store::LockOptions::default(),
    )
}

/// Switch only if the live Claude account still belongs to `expected`, so a
/// decision made on stale numbers never overrides a switch made by another
/// process in the meantime.
pub fn switch_profile_if_current(expected: &str, alias: &str) -> Result<bool> {
    validate_alias(expected)?;
    validate_alias(alias)?;
    let paths = crate::claude_usage::paths()?;
    let _lock = lock_live_auth()?;
    if crate::claude_usage::current_active().as_deref() != Some(expected) {
        return Ok(false);
    }
    crate::claude_store::switch_to(
        &paths,
        &app_home()?,
        alias,
        &crate::claude_store::LockOptions::default(),
    )?;
    Ok(true)
}

pub fn cmd_delete(alias: &str) -> Result<()> {
    validate_alias(alias)?;
    let _transaction = lock_auth_transaction()?;
    let dir = profiles_dir()?.join(alias);
    if !dir.exists() {
        return Err(CsError::NotFound(alias.to_string()).into());
    }
    if crate::claude_usage::current_active().as_deref() == Some(alias) {
        return Err(CsError::ActiveProfileDelete(alias.to_string()).into());
    }
    let deleted_dir = deleted_profiles_dir()?;
    ensure_private_dir(&deleted_dir)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    let archived = deleted_dir.join(format!("{alias}.backup-{timestamp}"));
    std::fs::rename(&dir, &archived).with_context(|| {
        format!(
            "archiving profile directory {} to {}",
            dir.display(),
            archived.display()
        )
    })?;
    user_println(&format!(
        "Deleted profile: {alias} (recoverable from {})",
        archived.display()
    ));
    Ok(())
}

/// Archived profiles as `(alias, archive dir name)`, newest first per alias.
pub fn list_deleted() -> Result<Vec<(String, String)>> {
    let dir = deleted_profiles_dir()?;
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|name| {
            let (alias, _) = name.rsplit_once(".backup-")?;
            Some((alias.to_string(), name))
        })
        .collect();
    // The suffix is a nanosecond timestamp, so reverse-name order is newest first.
    out.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    Ok(out)
}

/// Bring the newest archive of `alias` back as `new_alias` (default: `alias`).
pub fn cmd_restore(alias: &str, new_alias: Option<&str>) -> Result<String> {
    validate_alias(alias)?;
    let target = new_alias.unwrap_or(alias);
    validate_alias(target)?;
    let _transaction = lock_auth_transaction()?;
    let archive = list_deleted()?
        .into_iter()
        .find(|(a, _)| a == alias)
        .map(|(_, name)| deleted_profiles_dir().map(|d| d.join(name)))
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("no deleted profile named '{alias}'"))?;
    let dest = profiles_dir()?.join(target);
    if dest.exists() {
        anyhow::bail!("profile '{target}' already exists; use --as <new-alias>");
    }
    std::fs::rename(&archive, &dest)
        .with_context(|| format!("restoring {} to {}", archive.display(), dest.display()))?;
    Ok(target.to_string())
}

pub fn rename_profile(old_alias: &str, new_alias: &str) -> Result<()> {
    validate_alias(old_alias)?;
    validate_alias(new_alias)?;
    let old_dir = profiles_dir()?.join(old_alias);
    if !old_dir.exists() {
        return Err(CsError::NotFound(old_alias.to_string()).into());
    }
    let new_dir = profiles_dir()?.join(new_alias);
    if new_dir.exists() {
        anyhow::bail!("profile '{new_alias}' already exists");
    }
    let _transaction = lock_auth_transaction()?;
    std::fs::rename(&old_dir, &new_dir).with_context(|| {
        format!(
            "renaming profile {} -> {}",
            old_dir.display(),
            new_dir.display()
        )
    })?;
    if let Err(err) = crate::cache::rename(old_alias, new_alias) {
        tracing::warn!("Failed to rename cache entry {old_alias} -> {new_alias}: {err}");
    }
    if read_current() == old_alias {
        write_current(new_alias)?;
    }
    user_println(&format!("Renamed profile: {old_alias} -> {new_alias}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::MutexGuard;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use anyhow::Result;
    use fs4::FileExt;

    use super::{
        cmd_delete, rename_profile, switch_profile,
        switch_profile_if_current, validate_alias,
    };



    struct TestEnv {
        _lock: MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
        old_home: Option<OsString>,
        old_app_home: Option<OsString>,
        old_claude_config_dir: Option<OsString>,
    }

    struct ThreadCleanup<G> {
        blocker: Option<G>,
        workers: Vec<JoinHandle<()>>,
    }

    impl<G> ThreadCleanup<G> {
        fn new(blocker: G) -> Self {
            Self {
                blocker: Some(blocker),
                workers: Vec::new(),
            }
        }

        fn push(&mut self, worker: JoinHandle<()>) {
            self.workers.push(worker);
        }

        fn release_blocker(&mut self) {
            self.blocker.take();
        }

        fn join_all(&mut self) {
            let mut first_panic = None;
            for worker in self.workers.drain(..) {
                if let Err(panic) = worker.join()
                    && first_panic.is_none()
                {
                    first_panic = Some(panic);
                }
            }
            if let Some(panic) = first_panic {
                std::panic::resume_unwind(panic);
            }
        }
    }

    impl<G> Drop for ThreadCleanup<G> {
        fn drop(&mut self) {
            self.blocker.take();
            for worker in self.workers.drain(..) {
                let _ = worker.join();
            }
        }
    }

    impl TestEnv {
        fn new() -> Self {
            let lock = super::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let app_home = home.path().join(".paper-claude-switch");
            let old_home = std::env::var_os("HOME");
            let old_app_home = std::env::var_os("PAPER_CLAUDE_SWITCH_HOME");
            let old_claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");

            unsafe {
                std::env::set_var("CLAUDE_CONFIG_DIR", home.path().join(".claude"));
                std::env::set_var("HOME", home.path());
                std::env::set_var("PAPER_CLAUDE_SWITCH_HOME", &app_home);
            }

            Self {
                _lock: lock,
                _home: home,
                old_home,
                old_app_home,
                old_claude_config_dir,
            }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.old_home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match &self.old_app_home {
                    Some(value) => std::env::set_var("PAPER_CLAUDE_SWITCH_HOME", value),
                    None => std::env::remove_var("PAPER_CLAUDE_SWITCH_HOME"),
                }
                match &self.old_claude_config_dir {
                    Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
                    None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
                }
            }
        }
    }

    fn write_json_file(path: &std::path::Path, value: serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn claude_oauth(token: &str) -> serde_json::Value {
        serde_json::json!({
            "accessToken": token,
            "refreshToken": format!("refresh-{token}"),
            "expiresAt": 4_102_444_800_000_i64,
        })
    }

    fn claude_account(alias: &str) -> serde_json::Value {
        serde_json::json!({
            "accountUuid": format!("U-{alias}"),
            "emailAddress": format!("{alias}@example.com"),
        })
    }

    /// A saved Claude profile for account `U-<alias>`.
    fn seed_claude_profile(alias: &str, token: &str) {
        let dir = super::profiles_dir().unwrap().join(alias);
        write_json_file(
            &dir.join("credentials.json"),
            serde_json::json!({"claudeAiOauth": claude_oauth(token)}),
        );
        write_json_file(&dir.join("account.json"), claude_account(alias));
    }

    /// Claude Code logged in as account `U-<alias>`.
    fn write_live_claude(alias: &str, token: &str) {
        let paths = crate::claude_usage::paths().unwrap();
        write_json_file(
            &paths.credentials,
            serde_json::json!({"claudeAiOauth": claude_oauth(token)}),
        );
        write_json_file(
            &paths.global_config,
            serde_json::json!({"oauthAccount": claude_account(alias)}),
        );
    }

    fn live_claude_token() -> String {
        let live = crate::claude_store::read_live(&crate::claude_usage::paths().unwrap())
            .unwrap()
            .unwrap();
        live.oauth["accessToken"].as_str().unwrap().to_owned()
    }

    fn assert_invalid_alias(result: Result<()>, expected_message: &str) {
        let err = result.unwrap_err();
        assert_eq!(err.to_string(), expected_message);
    }

    #[test]
    fn validate_alias_accepts_expected_values() {
        assert!(validate_alias("alpha-123_.beta").is_ok());
        assert!(validate_alias(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn validate_alias_rejects_reserved_or_empty_values() {
        assert!(validate_alias("").is_err());
        assert!(validate_alias(".").is_err());
        assert!(validate_alias("..").is_err());
    }

    #[test]
    fn validate_alias_rejects_separators_and_non_ascii() {
        assert!(validate_alias("../escape").is_err());
        assert!(validate_alias("with/slash").is_err());
        assert!(validate_alias("\u{4E2D}\u{6587}").is_err());
        assert!(validate_alias(&"a".repeat(65)).is_err());
    }

    #[test]
    fn profile_commands_reject_invalid_alias_inputs() {
        let _env = TestEnv::new();

        for alias in ["../escape", "with/slash"] {
            assert_invalid_alias(
                switch_profile(alias).map(|_| ()),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
            assert_invalid_alias(
                cmd_delete(alias),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
            assert_invalid_alias(
                rename_profile(alias, "valid-alias"),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
        }

        assert_invalid_alias(switch_profile("").map(|_| ()), "alias cannot be empty");
        assert_invalid_alias(cmd_delete(""), "alias cannot be empty");
        assert_invalid_alias(rename_profile("", "valid-alias"), "alias cannot be empty");
    }

    #[test]
    fn rename_profile_rejects_invalid_new_alias() {
        let _env = TestEnv::new();
        let old_dir = super::profiles_dir().unwrap().join("valid-alias");
        std::fs::create_dir_all(&old_dir).unwrap();

        for alias in ["../escape", "with/slash"] {
            assert_invalid_alias(
                rename_profile("valid-alias", alias),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
        }

        assert_invalid_alias(rename_profile("valid-alias", ""), "alias cannot be empty");
    }

    #[test]
    fn switch_profile_waits_for_auth_lock() {
        let _env = TestEnv::new();

        seed_claude_profile("current", "acc_old");
        write_live_claude("current", "acc_old");
        seed_claude_profile("next-profile", "acc_new");

        let lock_path = super::auth_lock_path().unwrap();
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        FileExt::lock(&lock_file).unwrap();

        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("auth", attempt_tx);
            let _ = done_tx.send(super::switch_profile("next-profile"));
        });
        let mut cleanup = ThreadCleanup::new(lock_file);
        cleanup.push(handle);

        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not reach auth lock attempt");
        assert!(
            matches!(
                done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "switch should block while auth lock is held"
        );
        assert_eq!(live_claude_token(), "acc_old");

        cleanup.release_blocker();

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not finish after auth lock release")
            .unwrap();
        cleanup.join_all();
        assert_eq!(live_claude_token(), "acc_new");
        assert_eq!(super::read_current(), "next-profile");
    }

    #[test]
    fn conditional_switch_preserves_a_newer_manual_selection() {
        let _env = TestEnv::new();
        for (alias, access) in [("alpha", "access-a"), ("beta", "access-b"), ("charlie", "access-c")]
        {
            seed_claude_profile(alias, access);
        }

        switch_profile("alpha").unwrap();
        switch_profile("charlie").unwrap();

        assert!(!switch_profile_if_current("alpha", "beta").unwrap());
        assert_eq!(super::read_current(), "charlie");
        assert_eq!(live_claude_token(), "access-c");
        assert!(switch_profile_if_current("charlie", "beta").unwrap());
        assert_eq!(live_claude_token(), "access-b");
    }

    #[test]
    fn auth_lock_timeout_preserves_live_lock_inode() {
        let _env = TestEnv::new();
        let lock_path = super::auth_lock_path().unwrap();
        super::ensure_private_dir(lock_path.parent().unwrap()).unwrap();
        let holder = super::open_lock_file(&lock_path).unwrap();
        FileExt::lock(&holder).unwrap();
        super::write_lock_holder(&holder);

        let err =
            super::acquire_file_lock(&lock_path, Duration::from_millis(25), "auth").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("auth lock"), "{message}");
        assert!(
            message.contains(&lock_path.display().to_string()),
            "{message}"
        );

        let reopened = super::open_lock_file(&lock_path).unwrap();
        assert!(matches!(
            FileExt::try_lock(&reopened),
            Err(fs4::TryLockError::WouldBlock)
        ));
        FileExt::unlock(&holder).unwrap();
    }

    #[test]
    fn sync_current_from_live_matches_live_identity() {
        let _env = TestEnv::new();

        seed_claude_profile("alpha", "acc_a");
        seed_claude_profile("beta", "acc_b_old");

        super::write_current("alpha").unwrap();
        write_live_claude("beta", "acc_b_new");

        assert_eq!(super::sync_current_from_live().as_deref(), Some("beta"));
        assert_eq!(super::read_current(), "beta");
    }
}
