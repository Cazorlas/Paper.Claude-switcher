use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tempfile::NamedTempFile;

pub struct ClaudePaths {
    pub config_home: PathBuf,
    pub credentials: PathBuf,
    pub global_config: PathBuf,
}

impl ClaudePaths {
    pub fn resolve(config_dir: Option<&Path>, home: &Path) -> Self {
        let config_home = config_dir.map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".claude"));
        let legacy = config_home.join(".config.json");
        let global_config = if legacy.exists() {
            legacy
        } else if config_dir.is_some() {
            config_home.join(".claude.json")
        } else {
            home.join(".claude.json")
        };
        Self {
            credentials: config_home.join(".credentials.json"),
            config_home,
            global_config,
        }
    }
}

pub struct LockOptions {
    pub timeout: Duration,
    pub credentials_stale: Duration,
    pub config_stale: Duration,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(9),
            credentials_stale: Duration::from_secs(60),
            config_stale: Duration::from_secs(10),
        }
    }
}

pub struct LiveAccount {
    pub oauth: Value,
    pub account: Value,
    pub account_uuid: String,
    pub email: Option<String>,
    pub organization_uuid: Option<String>,
}

pub fn read_live(paths: &ClaudePaths) -> Result<Option<LiveAccount>> {
    let credentials = read_object(&paths.credentials)?;
    let config = read_object(&paths.global_config)?;
    live_account(&credentials, &config)
}

pub fn write_live(
    paths: &ClaudePaths,
    oauth: &Value,
    account: &Value,
    lock: &LockOptions,
) -> Result<()> {
    validate_account(oauth, account)?;
    let _locks = LiveLocks::acquire(paths, lock)?;
    let mut writes = WriteBatch::default();
    stage_live(&mut writes, paths, oauth, account)?;
    writes.commit()
}

#[derive(Debug, PartialEq, Eq)]
pub enum SaveAction {
    Created(String),
    Updated(String),
}

pub fn save_current(
    paths: &ClaudePaths,
    app_home: &Path,
    alias: Option<&str>,
    lock: &LockOptions,
) -> Result<SaveAction> {
    if let Some(alias) = alias {
        crate::profile::validate_alias(alias)?;
    }
    let _locks = LiveLocks::acquire(paths, lock)?;
    let live = read_live(paths)?.context("no live Claude account to save")?;
    let profiles = load_profiles(app_home)?;
    if let Some(named) = alias.and_then(|alias| profiles.iter().find(|p| p.alias == alias)) {
        if named.live.account_uuid != live.account_uuid {
            bail!("profile '{}' belongs to another account", named.alias);
        }
    }
    let current = read_current(app_home)?;
    let existing = find_account(&profiles, &live.account_uuid, current.as_deref());
    let (resolved, action) = if let Some(profile) = existing {
        (profile.alias.clone(), SaveAction::Updated(profile.alias.clone()))
    } else {
        let base = alias.map(str::to_owned).unwrap_or_else(|| inferred_alias(&live));
        let resolved = unique_alias(app_home, &base)?;
        (resolved.clone(), SaveAction::Created(resolved))
    };
    let mut writes = WriteBatch::default();
    stage_profile(&mut writes, app_home, &resolved, &live)?;
    writes.stage(&app_home.join("current"), resolved.as_bytes())?;
    writes.commit()?;
    Ok(action)
}

#[derive(Debug, PartialEq, Eq)]
pub enum SwitchOutcome {
    Switched { from: Option<String>, to: String },
    AlreadyActive,
}

pub fn switch_to(
    paths: &ClaudePaths,
    app_home: &Path,
    alias: &str,
    lock: &LockOptions,
) -> Result<SwitchOutcome> {
    crate::profile::validate_alias(alias)?;
    let _locks = LiveLocks::acquire(paths, lock)?;
    let target = load_profile(app_home, alias)?;
    let live = read_live(paths)?;
    if live.as_ref().is_some_and(|live| live.account_uuid == target.live.account_uuid) {
        return Ok(SwitchOutcome::AlreadyActive);
    }
    let profiles = load_profiles(app_home)?;
    let current = read_current(app_home)?;
    let leaving = match &live {
        Some(live) => Some(find_account(&profiles, &live.account_uuid, current.as_deref())
            .context("live Claude account is not saved; save it before switching")?),
        None => None,
    };
    let from = leaving.map(|profile| profile.alias.clone());
    let mut writes = WriteBatch::default();
    if let (Some(leaving), Some(live)) = (leaving, &live) {
        // Capture the token Claude Code may have rotated since the last save.
        stage_profile(&mut writes, app_home, &leaving.alias, live)?;
    }
    stage_live(&mut writes, paths, &target.live.oauth, &target.live.account)?;
    writes.stage(&app_home.join("current"), alias.as_bytes())?;
    writes.commit()?;
    Ok(SwitchOutcome::Switched { from, to: alias.to_owned() })
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn read_object(path: &Path) -> Result<Value> {
    let Some(bytes) = read_bytes(path)? else { return Ok(json!({})); };
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    if !value.is_object() {
        bail!("expected a JSON object in {}", path.display());
    }
    Ok(value)
}

fn account_uuid(account: &Value) -> Option<&str> {
    account.get("accountUuid").and_then(Value::as_str).filter(|uuid| !uuid.is_empty())
}

fn validate_account(oauth: &Value, account: &Value) -> Result<()> {
    if !oauth.is_object() {
        bail!("claudeAiOauth must be a JSON object");
    }
    if account_uuid(account).is_none() {
        bail!("oauthAccount must contain a non-empty accountUuid");
    }
    Ok(())
}

fn live_account(credentials: &Value, config: &Value) -> Result<Option<LiveAccount>> {
    let Some(oauth) = credentials.get("claudeAiOauth") else { return Ok(None); };
    let Some(account) = config.get("oauthAccount") else { return Ok(None); };
    let Some(uuid) = account_uuid(account) else { return Ok(None); };
    validate_account(oauth, account)?;
    Ok(Some(LiveAccount {
        oauth: oauth.clone(),
        account: account.clone(),
        account_uuid: uuid.to_owned(),
        email: account.get("emailAddress").and_then(Value::as_str).map(str::to_owned),
        organization_uuid: account.get("organizationUuid").and_then(Value::as_str).map(str::to_owned),
    }))
}

fn stage_live(writes: &mut WriteBatch, paths: &ClaudePaths, oauth: &Value, account: &Value) -> Result<()> {
    let mut credentials = read_object(&paths.credentials)?;
    let mut config = read_object(&paths.global_config)?;
    credentials["claudeAiOauth"] = oauth.clone();
    config["oauthAccount"] = account.clone();
    writes.stage_json(&paths.credentials, &credentials)?;
    writes.stage_json(&paths.global_config, &config)
}

struct SavedProfile {
    alias: String,
    live: LiveAccount,
}

fn load_profile(app_home: &Path, alias: &str) -> Result<SavedProfile> {
    crate::profile::validate_alias(alias)?;
    let dir = app_home.join("profiles").join(alias);
    let metadata = fs::symlink_metadata(&dir)
        .with_context(|| format!("profile '{alias}' not found"))?;
    if !metadata.file_type().is_dir() {
        bail!("profile '{alias}' must be a directory, not a symlink or file");
    }
    let credentials = read_object(&dir.join("credentials.json"))?;
    let account = read_object(&dir.join("account.json"))?;
    let live = live_account(&credentials, &json!({"oauthAccount": account}))?
        .with_context(|| format!("profile '{alias}' has no saved Claude account"))?;
    Ok(SavedProfile { alias: alias.to_owned(), live })
}

fn load_profiles(app_home: &Path) -> Result<Vec<SavedProfile>> {
    let dir = app_home.join("profiles");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    let mut profiles = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let alias = entry.file_name().into_string()
                .map_err(|_| anyhow::anyhow!("profile alias is not valid UTF-8"))?;
            profiles.push(load_profile(app_home, &alias)?);
        }
    }
    profiles.sort_by(|a, b| a.alias.cmp(&b.alias));
    Ok(profiles)
}

fn read_current(app_home: &Path) -> Result<Option<String>> {
    let Some(bytes) = read_bytes(&app_home.join("current"))? else { return Ok(None); };
    let marker = String::from_utf8(bytes).context("current profile marker is not UTF-8")?;
    let alias = marker.trim();
    if alias.is_empty() {
        return Ok(None);
    }
    crate::profile::validate_alias(alias)?;
    Ok(Some(alias.to_owned()))
}

fn find_account<'a>(profiles: &'a [SavedProfile], uuid: &str, current: Option<&str>) -> Option<&'a SavedProfile> {
    profiles.iter().find(|p| p.live.account_uuid == uuid && Some(p.alias.as_str()) == current)
        .or_else(|| profiles.iter().find(|p| p.live.account_uuid == uuid))
}

fn inferred_alias(live: &LiveAccount) -> String {
    let base = live.email.as_deref().and_then(|email| email.split('@').next()).unwrap_or("account");
    let alias: String = base.chars().take(64).map(|c| {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' }
    }).collect();
    if crate::profile::validate_alias(&alias).is_ok() { alias } else { "account".to_owned() }
}

fn unique_alias(app_home: &Path, base: &str) -> Result<String> {
    crate::profile::validate_alias(base)?;
    let dir = app_home.join("profiles");
    for n in 1..=1000 {
        let alias = if n == 1 {
            base.to_owned()
        } else {
            let suffix = format!("_{n}");
            format!("{}{}", &base[..base.len().min(64 - suffix.len())], suffix)
        };
        match fs::symlink_metadata(dir.join(&alias)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(alias),
            Err(error) => return Err(error).context("checking profile alias"),
            Ok(_) => {}
        }
    }
    bail!("could not generate a unique profile alias for '{base}'")
}

fn stage_profile(writes: &mut WriteBatch, app_home: &Path, alias: &str, live: &LiveAccount) -> Result<()> {
    let dir = app_home.join("profiles").join(alias);
    writes.stage_json(&dir.join("credentials.json"), &json!({"claudeAiOauth": live.oauth}))?;
    writes.stage_json(&dir.join("account.json"), &live.account)
}

// Only remove empty directories made by this operation, never existing directories.
#[derive(Default)]
struct CreatedDirectories(Vec<PathBuf>);

impl CreatedDirectories {
    fn ensure(&mut self, path: &Path) -> Result<()> {
        if path.as_os_str().is_empty() || path.is_dir() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            self.ensure(parent)?;
        }
        match fs::create_dir(path) {
            Ok(()) => {
                self.0.push(path.to_path_buf());
                #[cfg(windows)]
                crate::auth::harden_windows_private_directory(path)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
                }
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
            Err(error) => Err(error).with_context(|| format!("creating {}", path.display())),
        }
    }
}

impl Drop for CreatedDirectories {
    fn drop(&mut self) {
        for path in self.0.iter().rev() {
            let _ = fs::remove_dir(path);
        }
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

struct DirectoryLock {
    path: PathBuf,
    modified: SystemTime,
}

impl DirectoryLock {
    fn acquire(path: PathBuf, stale: Duration, started: Instant, timeout: Duration) -> Result<Self> {
        loop {
            match fs::create_dir(&path) {
                Ok(()) => {
                    let modified = match fs::metadata(&path).and_then(|m| m.modified()) {
                        Ok(modified) => modified,
                        Err(error) => {
                            let _ = fs::remove_dir(&path);
                            return Err(error).context("reading created lock directory");
                        }
                    };
                    return Ok(Self { path, modified });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata = match fs::symlink_metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error).with_context(|| format!("reading lock {}", path.display())),
                    };
                    if !metadata.file_type().is_dir() {
                        bail!("lock {} is not a directory", path.display());
                    }
                    let age = SystemTime::now().duration_since(metadata.modified()?).unwrap_or_default();
                    if age >= stale {
                        match fs::remove_dir(&path) {
                            Ok(()) => continue,
                            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                            Err(error) => return Err(error).with_context(|| format!("removing stale lock {}", path.display())),
                        }
                    }
                }
                Err(error) => return Err(error).with_context(|| format!("acquiring lock {}", path.display())),
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                bail!("timed out waiting for lock {}", path.display());
            }
            std::thread::sleep(remaining.min(Duration::from_millis(50)));
        }
    }
}

impl Drop for DirectoryLock {
    fn drop(&mut self) {
        // A stale-lock takeover can replace the directory while we are running.
        if fs::symlink_metadata(&self.path).ok().filter(|m| m.file_type().is_dir())
            .and_then(|m| m.modified().ok()) == Some(self.modified)
        {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

#[derive(Default)]
struct LiveLocks {
    locks: Vec<DirectoryLock>,
    directories: CreatedDirectories,
}

impl LiveLocks {
    fn acquire(paths: &ClaudePaths, options: &LockOptions) -> Result<Self> {
        let started = Instant::now();
        let mut held = Self::default();
        // Use Claude Code's order so a refresh and a swap cannot deadlock.
        for (path, stale) in [
            (paths.config_home.join(".oauth_refresh.lock"), options.credentials_stale),
            (lock_path(&paths.config_home), options.credentials_stale),
            (lock_path(&paths.global_config), options.config_stale),
        ] {
            held.directories.ensure(path.parent().context("lock path has no parent")?)?;
            held.locks.push(DirectoryLock::acquire(path, stale, started, options.timeout)?);
        }
        Ok(held)
    }
}

struct FileUpdate {
    path: PathBuf,
    staged: Option<NamedTempFile>,
    original: Option<NamedTempFile>,
}

#[derive(Default)]
struct WriteBatch {
    files: Vec<FileUpdate>,
    directories: CreatedDirectories,
}

fn private_temp(parent: &Path, bytes: &[u8]) -> Result<NamedTempFile> {
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    #[cfg(windows)]
    crate::auth::harden_windows_private_file(temp.path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(bytes).context("writing temporary account file")?;
    temp.as_file().sync_all().context("syncing temporary account file")?;
    Ok(temp)
}

impl WriteBatch {
    fn stage_json(&mut self, path: &Path, value: &Value) -> Result<()> {
        self.stage(path, &serde_json::to_vec_pretty(value)?)
    }

    fn stage(&mut self, path: &Path, bytes: &[u8]) -> Result<()> {
        let original = read_bytes(path)?;
        let parent = path.parent().context("account file has no parent")?;
        self.directories.ensure(parent)?;
        let original = original.as_deref().map(|bytes| private_temp(parent, bytes)).transpose()?;
        let staged = private_temp(parent, bytes)?;
        self.files.push(FileUpdate { path: path.to_path_buf(), staged: Some(staged), original });
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        // Stage every replacement and rollback copy before the first rename.
        for index in 0..self.files.len() {
            let file = &mut self.files[index];
            let result = file.staged.take().expect("staged write").persist(&file.path);
            if let Err(error) = result {
                let error = anyhow::Error::new(error.error)
                    .context(format!("atomically replacing {}", file.path.display()));
                let mut failures = Vec::new();
                for file in self.files[..index].iter_mut().rev() {
                    let result = match file.original.take() {
                        Some(original) => original.persist(&file.path).map(|_| ()).map_err(|e| e.error),
                        None => fs::remove_file(&file.path),
                    };
                    if let Err(restore_error) = result {
                        failures.push(format!("{}: {restore_error}", file.path.display()));
                    }
                }
                return if failures.is_empty() {
                    Err(error)
                } else {
                    Err(error.context(format!("account rollback failed: {}", failures.join("; "))))
                };
            }
        }
        Ok(())
    }
}
