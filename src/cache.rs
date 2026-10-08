use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::usage::{ResetCredit, UsageInfo};

static CACHE_LOCK: Mutex<()> = Mutex::new(());
const CACHE_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const CACHE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    ts: u64,
    primary_used: Option<f64>,
    primary_reset: Option<i64>,
    #[serde(default)]
    primary_window_minutes: Option<i64>,
    secondary_used: Option<f64>,
    secondary_reset: Option<i64>,
    #[serde(default)]
    secondary_window_minutes: Option<i64>,
    #[serde(default)]
    credits_balance: Option<f64>,
    #[serde(default)]
    unlimited_credits: Option<bool>,
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    reset_credits_available_count: Option<u64>,
    #[serde(default)]
    reset_credits: Vec<ResetCredit>,
    #[serde(default)]
    reset_credits_error: Option<String>,
    #[serde(default)]
    account_limited: bool,
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
    #[serde(default)]
    individual_limit: Option<Box<crate::usage::SpendControlLimit>>,
    #[serde(default)]
    additional_limits: Vec<crate::usage::AdditionalRateLimit>,
}

/// Last answer of the profile endpoint for one alias.
#[derive(Serialize, Deserialize, Clone)]
struct ProfileEntry {
    /// When the endpoint was last asked (unix seconds), failures included.
    checked_at: i64,
    subscription_status: Option<String>,
}

/// The profile status is asked for at most this often per alias.
const PROFILE_STATUS_TTL_SECS: i64 = 6 * 3600;

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    entries: HashMap<String, CacheEntry>,
    /// Tracks the last time each profile was selected by `use` (unix seconds).
    #[serde(default)]
    last_used: HashMap<String, i64>,
    /// Per alias: the usage endpoint must not be called before this instant
    /// (unix milliseconds), set from a 429 Retry-After.
    #[serde(default)]
    retry_until_ms: HashMap<String, i64>,
    #[serde(default)]
    profiles: HashMap<String, ProfileEntry>,
}

fn cache_path() -> Result<PathBuf> {
    Ok(auth::app_home()?.join("cache.json"))
}

fn cache_lock_path() -> Result<PathBuf> {
    Ok(auth::app_home()?.join("cache.lock"))
}

fn open_cache_lock_file(path: &std::path::Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating cache directory {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("setting permissions on {}", parent.display()))?;
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening cache lock {}", path.display()))
}

fn with_cache_file_lock_at<T>(
    path: &std::path::Path,
    timeout: Duration,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let file = open_cache_lock_file(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        match FileExt::try_lock(&file) {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(CACHE_LOCK_POLL_INTERVAL);
            }
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "cache lock {} remained held for {:.3}s; refusing to replace the live lock file",
                    path.display(),
                    timeout.as_secs_f64()
                );
            }
            Err(TryLockError::Error(err)) => {
                return Err(anyhow::Error::from(err))
                    .with_context(|| format!("locking cache file {}", path.display()));
            }
        }
    }
    operation()
}

fn with_cache_lock<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _process_lock = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("cache process lock poisoned"))?;
    with_cache_file_lock_at(&cache_lock_path()?, CACHE_LOCK_WAIT_TIMEOUT, operation)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ttl() -> u64 {
    crate::config::get().cache.ttl
}

fn load_cache() -> CacheFile {
    let path = match cache_path() {
        Ok(p) => p,
        Err(_) => return CacheFile::default(),
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &CacheFile) -> Result<()> {
    let path = cache_path()?;
    let json = serde_json::to_string(cache).context("serializing cache")?;
    auth::atomic_write_private(&path, json.as_bytes())
        .with_context(|| format!("writing cache file {}", path.display()))
}

fn to_entry(u: &UsageInfo) -> CacheEntry {
    CacheEntry {
        ts: u
            .fetched_at
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or_else(now_secs),
        primary_used: u.primary.as_ref().and_then(|w| w.used_percent),
        primary_reset: u.primary.as_ref().and_then(|w| w.resets_at),
        primary_window_minutes: u.primary.as_ref().and_then(|w| w.window_minutes),
        secondary_used: u.secondary.as_ref().and_then(|w| w.used_percent),
        secondary_reset: u.secondary.as_ref().and_then(|w| w.resets_at),
        secondary_window_minutes: u.secondary.as_ref().and_then(|w| w.window_minutes),
        credits_balance: u.credits_balance,
        unlimited_credits: u.unlimited_credits,
        plan_type: u.plan_type.clone(),
        reset_credits_available_count: u.reset_credits_available_count,
        reset_credits: u.reset_credits.clone(),
        reset_credits_error: u.reset_credits_error.clone(),
        account_limited: u.account_limited,
        rate_limit_reached_type: u.rate_limit_reached_type.clone(),
        individual_limit: u.individual_limit.clone(),
        additional_limits: u.additional_limits.clone(),
    }
}

fn from_entry(e: &CacheEntry) -> UsageInfo {
    use crate::usage::WindowUsage;
    let primary = if e.primary_used.is_some() || e.primary_reset.is_some() {
        Some(WindowUsage {
            used_percent: e.primary_used,
            resets_at: e.primary_reset,
            window_minutes: e.primary_window_minutes,
        })
    } else {
        None
    };
    let secondary = if e.secondary_used.is_some() || e.secondary_reset.is_some() {
        Some(WindowUsage {
            used_percent: e.secondary_used,
            resets_at: e.secondary_reset,
            window_minutes: e.secondary_window_minutes,
        })
    } else {
        None
    };
    UsageInfo {
        fetched_at: Some(e.ts as i64),
        primary,
        secondary,
        credits_balance: e.credits_balance,
        unlimited_credits: e.unlimited_credits,
        plan_type: e.plan_type.clone(),
        reset_credits_available_count: e.reset_credits_available_count,
        reset_credits: e.reset_credits.clone(),
        reset_credits_error: e.reset_credits_error.clone(),
        account_limited: e.account_limited,
        rate_limit_reached_type: e.rate_limit_reached_type.clone(),
        individual_limit: e.individual_limit.clone(),
        additional_limits: e.additional_limits.clone(),
        subscription_status: None,
    }
}

/// The cached usage of `alias` with its subscription status filled in.
fn usage_of(cache: &CacheFile, alias: &str) -> Option<UsageInfo> {
    let mut usage = from_entry(cache.entries.get(alias)?);
    usage.subscription_status = cache
        .profiles
        .get(alias)
        .and_then(|profile| profile.subscription_status.clone());
    Some(usage)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Get cached usage for an alias if within TTL.
pub fn get(alias: &str) -> Option<UsageInfo> {
    match with_cache_lock(|| {
        let cache = load_cache();
        let Some(entry) = cache.entries.get(alias) else {
            return Ok(None);
        };
        if now_secs().saturating_sub(entry.ts) > ttl() {
            return Ok(None);
        }
        Ok(usage_of(&cache, alias))
    }) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("Failed to read cache for {alias}: {err}");
            None
        }
    }
}

/// The last usage stored for an alias, however old.
pub fn last_good(alias: &str) -> Option<UsageInfo> {
    match with_cache_lock(|| Ok(usage_of(&load_cache(), alias))) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("Failed to read cache for {alias}: {err}");
            None
        }
    }
}

/// Store usage result in cache. A good reading ends any rate-limit pause.
pub fn put(alias: &str, usage: &UsageInfo) {
    if let Err(err) = with_cache_lock(|| {
        let mut cache = load_cache();
        cache.entries.insert(alias.to_string(), to_entry(usage));
        cache.retry_until_ms.remove(alias);
        save_cache(&cache)
    }) {
        tracing::warn!("Failed to write cache: {err}");
    }
}

/// Milliseconds left of the rate-limit pause of `alias`, when one is running.
pub fn pause_left_ms(alias: &str) -> Option<i64> {
    match with_cache_lock(|| Ok(load_cache().retry_until_ms.get(alias).copied())) {
        Ok(until) => until.map(|until| until - now_millis()).filter(|left| *left > 0),
        Err(err) => {
            tracing::warn!("Failed to read rate-limit pause for {alias}: {err}");
            None
        }
    }
}

/// Do not call the usage endpoint for `alias` for the next `wait_ms`.
pub fn pause_for_ms(alias: &str, wait_ms: i64) {
    if let Err(err) = with_cache_lock(|| {
        let mut cache = load_cache();
        cache.retry_until_ms.insert(alias.to_string(), now_millis().saturating_add(wait_ms));
        save_cache(&cache)
    }) {
        tracing::warn!("Failed to write rate-limit pause: {err}");
    }
}

/// True when the profile status of `alias` was never asked for or is older than 6 h.
pub fn profile_status_due(alias: &str) -> bool {
    match with_cache_lock(|| Ok(load_cache().profiles.get(alias).map(|p| p.checked_at))) {
        Ok(Some(checked)) => now_secs() as i64 - checked >= PROFILE_STATUS_TTL_SECS,
        Ok(None) => true,
        Err(_) => false,
    }
}

/// Record a profile-endpoint attempt. `status` is the new subscription status
/// on success; `None` (a failure) keeps the previous one.
pub fn put_profile_status(alias: &str, status: Option<String>) {
    if let Err(err) = with_cache_lock(|| {
        let mut cache = load_cache();
        let previous = cache.profiles.get(alias).and_then(|p| p.subscription_status.clone());
        cache.profiles.insert(
            alias.to_string(),
            ProfileEntry {
                checked_at: now_secs() as i64,
                subscription_status: status.or(previous),
            },
        );
        save_cache(&cache)
    }) {
        tracing::warn!("Failed to write profile status: {err}");
    }
}

/// The cached subscription status of `alias`, however old.
pub fn profile_status(alias: &str) -> Option<String> {
    with_cache_lock(|| {
        Ok(load_cache()
            .profiles
            .get(alias)
            .and_then(|profile| profile.subscription_status.clone()))
    })
    .ok()
    .flatten()
}

/// Move every record keyed by `old` over to `new`. Returns whether anything moved.
fn migrate_alias(cache: &mut CacheFile, old: &str, new: &str) -> bool {
    // A profile can have been used but never fetched, so each map is checked.
    let mut changed = false;
    if let Some(entry) = cache.entries.remove(old) {
        cache.entries.insert(new.to_string(), entry);
        changed = true;
    }
    if let Some(ts) = cache.last_used.remove(old) {
        cache.last_used.insert(new.to_string(), ts);
        changed = true;
    }
    if let Some(until) = cache.retry_until_ms.remove(old) {
        cache.retry_until_ms.insert(new.to_string(), until);
        changed = true;
    }
    if let Some(profile) = cache.profiles.remove(old) {
        cache.profiles.insert(new.to_string(), profile);
        changed = true;
    }
    changed
}

/// Get the last-used timestamp for an alias (0 if never used).
pub fn get_last_used(alias: &str) -> i64 {
    match with_cache_lock(|| Ok(load_cache().last_used.get(alias).copied().unwrap_or(0))) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("Failed to read last-used cache for {alias}: {err}");
            0
        }
    }
}

/// Record that an alias was just selected by `use`.
pub fn set_last_used(alias: &str) -> Result<()> {
    with_cache_lock(|| {
        let mut cache = load_cache();
        cache
            .last_used
            .insert(alias.to_string(), crate::auth::now_unix_secs());
        save_cache(&cache).context("writing last_used cache")
    })
}

pub fn rename(old: &str, new: &str) -> Result<()> {
    with_cache_lock(|| {
        let mut cache = load_cache();
        if migrate_alias(&mut cache, old, new) {
            save_cache(&cache)?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs4::FileExt;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn test_cache_entry_deserialize_without_optional_fields() {
        let entry: CacheEntry = serde_json::from_value(json!({
            "ts": 123,
            "primary_used": 25.0,
            "primary_reset": 456,
            "secondary_used": null,
            "secondary_reset": null
        }))
        .unwrap();
        let usage = from_entry(&entry);
        assert_eq!(usage.primary.unwrap().used_percent, Some(25.0));
        assert!(usage.secondary.is_none());
        assert!(!usage.account_limited);
    }

    #[test]
    fn migrate_alias_moves_usage_and_last_used() {
        let mut cache = CacheFile::default();
        cache.last_used.insert("old".into(), 7);
        cache.entries.insert(
            "old".into(),
            to_entry(&UsageInfo {
                fetched_at: Some(5),
                ..Default::default()
            }),
        );

        assert!(migrate_alias(&mut cache, "old", "new"));
        assert!(!cache.entries.contains_key("old"));
        assert!(cache.entries.contains_key("new"));
        assert_eq!(cache.last_used.get("new"), Some(&7));
        assert!(!migrate_alias(&mut cache, "missing", "other"));
    }

    #[test]
    fn cache_mutation_waits_for_cross_process_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("cache.lock");
        let holder = open_cache_lock_file(&lock_path).unwrap();
        FileExt::lock(&holder).unwrap();

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_path = lock_path.clone();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(with_cache_file_lock_at(
                    &worker_path,
                    Duration::from_secs(1),
                    || Ok(()),
                ))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "cache mutation must wait for an independently-held OS lock"
        );

        drop(holder);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn cache_lock_timeout_preserves_live_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("cache.lock");
        let holder = open_cache_lock_file(&lock_path).unwrap();
        std::fs::write(&lock_path, "holder-marker").unwrap();
        FileExt::lock(&holder).unwrap();

        let err =
            with_cache_file_lock_at(&lock_path, Duration::from_millis(25), || Ok(())).unwrap_err();
        assert!(err.to_string().contains("cache lock"));
        let reopened = open_cache_lock_file(&lock_path).unwrap();
        assert!(matches!(
            FileExt::try_lock(&reopened),
            Err(fs4::TryLockError::WouldBlock)
        ));
        FileExt::unlock(&holder).unwrap();
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            "holder-marker"
        );
    }

    #[test]
    fn cache_round_trip_keeps_per_model_weekly_windows() {
        let usage = UsageInfo {
            fetched_at: Some(10),
            additional_limits: vec![crate::usage::AdditionalRateLimit {
                limit_name: Some("Fable".to_string()),
                secondary: Some(crate::usage::WindowUsage {
                    used_percent: Some(100.0),
                    resets_at: Some(999),
                    window_minutes: Some(10080),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let restored = from_entry(&to_entry(&usage));
        assert_eq!(restored.additional_limits.len(), 1);
        let model = &restored.additional_limits[0];
        assert_eq!(model.limit_name.as_deref(), Some("Fable"));
        let window = model.secondary.as_ref().expect("weekly window kept");
        assert_eq!(window.used_percent, Some(100.0));
        assert_eq!(window.resets_at, Some(999));
    }
}
