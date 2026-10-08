use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crate::error::CsError;

/// User-Agent: `paper-claude-switch/<version> (<os>; <arch>)`.
pub(crate) fn user_agent() -> String {
    format!(
        "paper-claude-switch/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// The user's Claude config directory (`$CLAUDE_CONFIG_DIR`, or `~/.claude`).
/// It belongs to Claude Code, so this app never changes its permissions.
#[cfg(windows)]
fn claude_config_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home.join(".claude"))
}

/// ~/.paper-claude-switch/
pub fn app_home() -> Result<PathBuf> {
    // Keep application state relocatable without changing Claude's own home.
    if let Some(path) = std::env::var_os("PAPER_CLAUDE_SWITCH_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home.join(".paper-claude-switch"))
}

/// ~/.paper-claude-switch/profiles/
pub fn profiles_dir() -> Result<PathBuf> {
    Ok(app_home()?.join("profiles"))
}

/// ~/.paper-claude-switch/current
pub fn current_file() -> Result<PathBuf> {
    Ok(app_home()?.join("current"))
}

pub fn read_auth(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        return Err(CsError::NoAuthFile(path.display().to_string()).into());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let val: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(val)
}

pub(crate) fn atomic_write_private(path: &Path, contents: &[u8]) -> Result<()> {
    #[cfg(windows)]
    {
        let shared_home = claude_config_home().ok();
        let owned_home = app_home().ok();
        atomic_write_private_inner(
            path,
            contents,
            shared_home.as_deref(),
            owned_home.as_deref(),
        )
    }
    #[cfg(not(windows))]
    atomic_write_private_inner(path, contents)
}

fn atomic_write_private_inner(
    path: &Path,
    contents: &[u8],
    #[cfg(windows)] shared_home: Option<&Path>,
    #[cfg(windows)] owned_home: Option<&Path>,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    #[cfg(windows)]
    let harden_parent =
        { should_harden_windows_parent(parent, shared_home, owned_home, parent.is_dir()) };
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;
    #[cfg(windows)]
    if harden_parent {
        harden_windows_acl(parent, true)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", parent.display()))?;
    }

    #[cfg(windows)]
    let mut tmp = create_private_windows_temp(parent)
        .with_context(|| format!("creating protected temporary file in {}", parent.display()))?;
    #[cfg(not(windows))]
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", tmp.path().display()))?;
    }
    tmp.write_all(contents)
        .with_context(|| format!("writing temporary file for {}", path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary file for {}", path.display()))?;
    tmp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("atomically replacing {}", path.display()))?;
    #[cfg(windows)]
    harden_windows_acl(path, false)?;
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_private_acl_sddl(current_user_sid: &str, directory: bool) -> String {
    let inheritance = if directory { "OICI" } else { "" };
    format!(
        "D:P(A;{inheritance};FA;;;{current_user_sid})\
         (A;{inheritance};FA;;;S-1-5-18)\
         (A;{inheritance};FA;;;S-1-5-32-544)"
    )
}

#[cfg(windows)]
fn should_harden_windows_parent(
    parent: &Path,
    claude_home: Option<&Path>,
    owned_app_home: Option<&Path>,
    existed_before_write: bool,
) -> bool {
    if !existed_before_write || !parent.is_dir() {
        return true;
    }

    // Leave Claude Code's own config directory alone only when all three
    // existing paths resolve. Canonical paths avoid treating a sibling such as
    // `paper-claude-switch-old` as a descendant of `paper-claude-switch`;
    // resolution errors fail closed and retain directory hardening.
    let (Some(shared), Some(owned)) = (claude_home, owned_app_home) else {
        return true;
    };
    let (Ok(parent_real), Ok(shared_real), Ok(owned_real)) = (
        parent.canonicalize(),
        shared.canonicalize(),
        owned.canonicalize(),
    ) else {
        return true;
    };

    // App-owned paths take precedence over the shared-home exception.
    if parent_real != shared_real {
        return true;
    }
    parent_real.starts_with(owned_real)
}

#[cfg(windows)]
fn harden_windows_acl(path: &Path, directory: bool) -> Result<()> {
    windows_acl_security_descriptor(path, directory, true).map(|_| ())
}

#[cfg(windows)]
fn windows_acl_security_descriptor(
    path: &Path,
    directory: bool,
    apply: bool,
) -> Result<*mut core::ffi::c_void> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The process-token SID walk and the SDDL → security-descriptor
    /// conversion are identical on every call: cache both.  Only the
    /// `SetNamedSecurityInfoW` write must run per file, which is also the
    /// call a slow filesystem or antivirus makes expensive.
    struct AclParts {
        dir_sd: PSECURITY_DESCRIPTOR,
        file_sd: PSECURITY_DESCRIPTOR,
    }

    unsafe impl Send for AclParts {}
    unsafe impl Sync for AclParts {}

    static ACL_PARTS: std::sync::OnceLock<Result<AclParts, String>> = std::sync::OnceLock::new();

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper is only constructed from a successful
            // OpenProcessToken call and owns that handle exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    struct LocalAllocation(*mut core::ffi::c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            // SAFETY: both wrapped pointers come from Win32 APIs documented to
            // allocate with LocalAlloc and are released exactly once here.
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn last_error(path: &Path, api: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "{api} failed for {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )
    }

    let parts = ACL_PARTS.get_or_init(|| -> Result<AclParts, String> {
        (|| -> Result<AclParts> {
            let mut token = null_mut();
            // SAFETY: GetCurrentProcess returns a valid pseudo-handle, and
            // `token` points to writable storage for the owned token handle.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                return Err(last_error(path, "OpenProcessToken"));
            }
            let _token = OwnedHandle(token);

            let mut token_user_bytes = 0;
            // SAFETY: the null-buffer probe is the documented way to obtain
            // the TOKEN_USER size; no output buffer is dereferenced.
            let probe_ok = unsafe {
                GetTokenInformation(token, TokenUser, null_mut(), 0, &mut token_user_bytes)
            };
            let probe_error = std::io::Error::last_os_error();
            if probe_ok != 0
                || token_user_bytes == 0
                || probe_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            {
                return Err(anyhow::anyhow!(
                    "GetTokenInformation(TokenUser size) failed for {}: {probe_error}",
                    path.display()
                ));
            }

            let words = (token_user_bytes as usize).div_ceil(std::mem::size_of::<usize>());
            let mut token_user = vec![0usize; words];
            // SAFETY: the usize-backed buffer is suitably aligned for
            // TOKEN_USER and has the exact byte capacity from the size probe.
            if unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    token_user.as_mut_ptr().cast(),
                    token_user_bytes,
                    &mut token_user_bytes,
                )
            } == 0
            {
                return Err(last_error(path, "GetTokenInformation(TokenUser)"));
            }
            // SAFETY: GetTokenInformation initialized the aligned buffer as
            // TOKEN_USER, and the SID stays valid while `token_user` lives.
            let user_sid = unsafe { (*(token_user.as_ptr().cast::<TOKEN_USER>())).User.Sid };

            let mut string_sid = null_mut();
            // SAFETY: `user_sid` comes from the live TOKEN_USER buffer and the
            // API writes a LocalAlloc-owned, NUL-terminated UTF-16 pointer.
            if unsafe { ConvertSidToStringSidW(user_sid, &mut string_sid) } == 0 {
                return Err(last_error(path, "ConvertSidToStringSidW"));
            }
            let _string_sid = LocalAllocation(string_sid.cast());
            let mut sid_len = 0;
            // SAFETY: ConvertSidToStringSidW guarantees a NUL-terminated UTF-16
            // string, and `_string_sid` keeps that allocation alive.
            while unsafe { *string_sid.add(sid_len) } != 0 {
                sid_len += 1;
            }
            // SAFETY: `sid_len` was found within the API-provided allocation
            // and excludes the terminator.
            let current_user_sid =
                String::from_utf16(unsafe { std::slice::from_raw_parts(string_sid, sid_len) })
                    .with_context(|| {
                        format!(
                            "decoding ConvertSidToStringSidW output for {}",
                            path.display()
                        )
                    })?;

            let make_sd = |directory: bool| -> Result<PSECURITY_DESCRIPTOR> {
                let sddl = windows_private_acl_sddl(&current_user_sid, directory);
                let sddl_wide: Vec<u16> = std::ffi::OsStr::new(&sddl)
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
                let mut sd: PSECURITY_DESCRIPTOR = null_mut();
                // SAFETY: `sddl_wide` is NUL-terminated and `sd` is writable;
                // the returned descriptor is intentionally leaked through
                // ACL_PARTS so every call reuses the same read-only memory.
                if unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        sddl_wide.as_ptr(),
                        SDDL_REVISION_1,
                        &mut sd,
                        null_mut(),
                    )
                } == 0
                {
                    return Err(last_error(
                        path,
                        "ConvertStringSecurityDescriptorToSecurityDescriptorW",
                    ));
                }
                Ok(sd)
            };
            Ok(AclParts {
                dir_sd: make_sd(true)?,
                file_sd: make_sd(false)?,
            })
        })()
        .map_err(|error| format!("{error:#}"))
    });
    let security_descriptor = match parts {
        Ok(parts) => {
            if directory {
                parts.dir_sd
            } else {
                parts.file_sd
            }
        }
        Err(error) => return Err(anyhow::anyhow!("{error}")),
    };

    let mut dacl_present = 0;
    let mut dacl: *mut ACL = null_mut();
    let mut dacl_defaulted = 0;
    // SAFETY: `security_descriptor` is the cached, immutable descriptor; all
    // output pointers refer to initialized local variables.
    if unsafe {
        GetSecurityDescriptorDacl(
            security_descriptor,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
    {
        return Err(last_error(path, "GetSecurityDescriptorDacl"));
    }
    if dacl_present == 0 || dacl.is_null() {
        anyhow::bail!(
            "GetSecurityDescriptorDacl returned no DACL for {}",
            path.display()
        );
    }

    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Writing a directory DACL makes Windows re-propagate inheritance through
    // the whole tree below it. A large tree (thousands of entries) made that
    // take seconds on
    // every write. Skip the write when the exact protected DACL is already in
    // place; anything else, including an extra or missing ACE, is rewritten.
    if apply && windows_dacl_already_matches(&path_wide, dacl) {
        tracing::debug!(
            path = %path.display(),
            directory,
            "windows ACL already hardened"
        );
        return Ok(security_descriptor);
    }

    if !apply {
        return Ok(security_descriptor);
    }

    let acl_write_start = std::time::Instant::now();
    // SAFETY: the path is NUL-terminated, `dacl` points inside the live
    // security descriptor, and null owner/group/SACL pointers are required
    // because only the exact protected DACL is being replaced.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        )
    };
    let acl_ms = acl_write_start.elapsed().as_millis() as u64;
    if acl_ms >= 500 {
        tracing::warn!(
            path = %path.display(),
            directory,
            acl_ms,
            "windows ACL write is unusually slow; check OneDrive/AV on the profile directory"
        );
    }
    tracing::debug!(
        path = %path.display(),
        directory,
        acl_ms,
        "hardened windows ACL"
    );
    if status != ERROR_SUCCESS {
        return Err(anyhow::anyhow!(
            "SetNamedSecurityInfoW failed for {}: {}",
            path.display(),
            std::io::Error::from_raw_os_error(status as i32)
        ));
    }

    Ok(security_descriptor)
}

#[cfg(windows)]
fn create_private_windows_temp(parent: &Path) -> Result<tempfile::NamedTempFile<std::fs::File>> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    };

    tempfile::Builder::new()
        .prefix(".paper-claude-switch-auth-")
        .make_in(parent, |candidate| {
            let security_descriptor = windows_acl_security_descriptor(candidate, false, false)
                .map_err(|error| {
                    std::io::Error::other(format!("preparing protected ACL: {error:#}"))
                })?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: security_descriptor,
                bInheritHandle: 0,
            };
            let candidate_wide: Vec<u16> = candidate
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SECURITY_ATTRIBUTES applies the exact protected DACL when the file is created.
            // Its descriptor points to the process-lifetime cached SD; the
            // attributes and NUL-terminated path remain live for this call.
            let handle = unsafe {
                CreateFileW(
                    candidate_wide.as_ptr(),
                    windows_sys::Win32::Foundation::GENERIC_READ
                        | windows_sys::Win32::Foundation::GENERIC_WRITE,
                    FILE_SHARE_DELETE | FILE_SHARE_READ,
                    &attributes,
                    CREATE_NEW,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: CreateFileW returned a uniquely owned file handle which
            // is transferred to File and closed exactly once by its Drop.
            Ok(unsafe { std::fs::File::from_raw_handle(handle.cast()) })
        })
        .map_err(anyhow::Error::from)
}

/// Whether the object at `path_wide` already carries a protected DACL whose
/// ACEs are byte-identical, in order, to `desired`. Any read failure answers
/// `false`, so the caller falls back to writing the DACL.
#[cfg(windows)]
fn windows_dacl_already_matches(
    path_wide: &[u16],
    desired: *const windows_sys::Win32::Security::ACL,
) -> bool {
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
        PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };

    /// Raw bytes of every ACE in `acl`, or `None` when one cannot be read.
    fn ace_bytes(acl: *const ACL) -> Option<Vec<Vec<u8>>> {
        // SAFETY: `acl` is a live ACL; its header is read-only here.
        let count = unsafe { (*acl).AceCount } as u32;
        let mut aces = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut ace = null_mut();
            // SAFETY: `index` is below AceCount and `ace` is writable storage.
            if unsafe { GetAce(acl, index, &mut ace) } == 0 || ace.is_null() {
                return None;
            }
            // SAFETY: GetAce returned a pointer to an ACE inside `acl`, which
            // starts with an ACE_HEADER whose AceSize covers the whole ACE.
            let size = unsafe { (*ace.cast::<ACE_HEADER>()).AceSize } as usize;
            aces.push(unsafe { std::slice::from_raw_parts(ace.cast::<u8>(), size) }.to_vec());
        }
        Some(aces)
    }

    let mut current: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `path_wide` is NUL-terminated; only the DACL is requested and
    // the returned descriptor is freed below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut current,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return false;
    }
    let matches = (|| {
        if current.is_null() {
            return false;
        }
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: `descriptor` is the live descriptor returned above.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
            || control & SE_DACL_PROTECTED == 0
        {
            return false;
        }
        match (ace_bytes(current), ace_bytes(desired)) {
            (Some(current), Some(desired)) => current == desired,
            _ => false,
        }
    })();
    // SAFETY: GetNamedSecurityInfoW allocated `descriptor` with LocalAlloc;
    // `current` points into it and is not used after this point.
    unsafe {
        LocalFree(descriptor);
    }
    matches
}

#[cfg(windows)]
pub(crate) fn harden_windows_private_directory(path: &Path) -> Result<()> {
    harden_windows_acl(path, true)
}

#[cfg(windows)]
pub(crate) fn harden_windows_private_file(path: &Path) -> Result<()> {
    harden_windows_acl(path, false)
}

/// Current unix timestamp in seconds.
pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build a shared reqwest client with standard user-agent and proxy support.
pub fn build_http_client() -> Result<reqwest::Client> {
    let proxy_url = crate::config::resolve_proxy();
    build_http_client_with_proxy(proxy_url.as_deref())
}

pub fn build_http_client_with_proxy(proxy_url: Option<&str>) -> Result<reqwest::Client> {
    build_http_client_with_proxy_and_redirect_policy(
        proxy_url,
        reqwest::redirect::Policy::default(),
    )
}

pub(crate) fn build_http_client_with_proxy_and_redirect_policy(
    proxy_url: Option<&str>,
    redirect_policy: reqwest::redirect::Policy,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(60))
        .redirect(redirect_policy);

    if let Some(url) = proxy_url {
        let sanitized_url = sanitize_proxy_url(url);
        tracing::debug!("Using proxy: {sanitized_url}");
        let mut proxy = reqwest::Proxy::all(url)
            .map_err(|e| anyhow::anyhow!("invalid proxy URL '{sanitized_url}': {e}"))?;
        if let Some(no_proxy) = crate::config::resolve_no_proxy() {
            tracing::debug!("No-proxy list: {no_proxy}");
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&no_proxy));
        }
        builder = builder.proxy(proxy);
    }

    if let Some(path) = custom_ca_path_from_values([
        std::env::var_os("CS_CA_CERTIFICATE"),
        std::env::var_os("NODE_EXTRA_CA_CERTS"),
        std::env::var_os("SSL_CERT_FILE"),
    ]) {
        let pem = std::fs::read(&path)
            .with_context(|| format!("reading custom CA bundle {}", path.display()))?;
        let certificates = reqwest::Certificate::from_pem_bundle(&pem)
            .with_context(|| format!("parsing custom CA bundle {}", path.display()))?;
        if certificates.is_empty() {
            anyhow::bail!(
                "custom CA bundle {} contains no certificates",
                path.display()
            );
        }
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }

    Ok(builder.build()?)
}

/// The first non-empty CA bundle path, in priority order: this tool's own
/// variable, then the one Claude Code reads, then the OpenSSL convention.
fn custom_ca_path_from_values(values: [Option<OsString>; 3]) -> Option<PathBuf> {
    values
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn sanitize_proxy_url(url: &str) -> String {
    let Some(scheme_sep) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_sep + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map(|idx| authority_start + idx)
        .unwrap_or(url.len());
    let authority = &url[authority_start..authority_end];
    let Some(userinfo_end) = authority.rfind('@') else {
        return url.to_string();
    };
    let at_pos = authority_start + userinfo_end;

    let mut sanitized = String::with_capacity(url.len());
    sanitized.push_str(&url[..authority_start]);
    sanitized.push_str("***:***");
    sanitized.push_str(&url[at_pos..]);
    sanitized
}

/// An intercepting proxy re-signs traffic with its own CA, and rustls reports
/// that as a bare "UnknownIssuer" with no indication of what to do. The OS trust
/// store is consulted first, so reaching here means the CA is not installed
/// there either and has to be supplied explicitly.
fn tls_trust_hint(message: &str) -> Option<&'static str> {
    if message.contains("UnknownIssuer") || message.contains("invalid peer certificate") {
        return Some(
            "\n  hint: the server's certificate was not signed by a CA this machine trusts. \
             An intercepting proxy (Proxyman, Charles, a corporate MITM) re-signs traffic with \
             its own CA — add that CA to the system trust store, or export it as PEM and point \
             CS_CA_CERTIFICATE at the file.",
        );
    }
    None
}

/// Format a reqwest error with the full source chain for diagnostics.
pub fn format_reqwest_error(context: &str, err: &reqwest::Error) -> anyhow::Error {
    let mut msg = format!("{context}: {err}");
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        msg.push_str(&format!("\n  caused by: {cause}"));
        source = std::error::Error::source(cause);
    }
    if let Some(hint) = tls_trust_hint(&msg) {
        msg.push_str(hint);
    }
    anyhow::anyhow!("{msg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_names_this_tool() {
        let ua = user_agent();
        assert!(ua.starts_with("paper-claude-switch/"), "unexpected UA: {ua}");
        assert!(ua.ends_with(')'));
    }

    #[test]
    fn test_sanitize_proxy_url_masks_userinfo() {
        let url = "http://user:pass@example.com:8080/path?q=1";

        assert_eq!(
            sanitize_proxy_url(url),
            "http://***:***@example.com:8080/path?q=1"
        );
    }

    #[test]
    fn test_sanitize_proxy_url_keeps_url_without_userinfo() {
        let url = "socks5://example.com:1080";

        assert_eq!(sanitize_proxy_url(url), url);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_private_write_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");

        atomic_write_private(&path, br#"{"claudeAiOauth":{}}"#).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn windows_acl_sddl_replaces_the_dacl_instead_of_only_removing_inheritance() {
        let sddl = windows_private_acl_sddl("S-1-5-21-1-2-3-1001", true);
        assert!(sddl.starts_with("D:P"));
        assert_eq!(sddl.matches("(A;").count(), 3);
        assert!(
            !sddl.contains("S-1-1-0"),
            "the exact DACL path must not preserve unknown explicit ACEs"
        );
    }

    #[test]
    fn custom_ca_takes_the_first_non_empty_variable() {
        let own = custom_ca_path_from_values([
            Some(OsString::from("/certs/own.pem")),
            Some(OsString::from("/certs/node.pem")),
            Some(OsString::from("/certs/ssl.pem")),
        ]);
        assert_eq!(own, Some(PathBuf::from("/certs/own.pem")));

        let node = custom_ca_path_from_values([
            Some(OsString::from("")),
            Some(OsString::from("/certs/node.pem")),
            Some(OsString::from("/certs/ssl.pem")),
        ]);
        assert_eq!(node, Some(PathBuf::from("/certs/node.pem")));

        let ssl = custom_ca_path_from_values([None, None, Some(OsString::from("/certs/ssl.pem"))]);
        assert_eq!(ssl, Some(PathBuf::from("/certs/ssl.pem")));
        assert_eq!(custom_ca_path_from_values([None, Some(OsString::new()), None]), None);
    }

    #[test]
    fn unknown_issuer_error_explains_how_to_trust_an_intercepting_proxy() {
        let msg = "Usage API request failed: error sending request\n  caused by: invalid peer certificate: UnknownIssuer";
        let hint = super::tls_trust_hint(msg).expect("UnknownIssuer must carry a hint");
        assert!(
            hint.contains("CS_CA_CERTIFICATE"),
            "the hint must name the variable that fixes it: {hint}"
        );
    }

    #[test]
    fn an_ordinary_connection_failure_gets_no_certificate_hint() {
        let msg = "Usage API request failed: error sending request\n  caused by: tcp connect error: Connection refused (os error 61)";
        assert!(
            super::tls_trust_hint(msg).is_none(),
            "a hint about certificates would misdirect a plain connection failure"
        );
    }

    #[test]
    fn windows_private_acl_sddl_is_exact_and_language_neutral() {
        let current_user = "S-1-5-21-1-2-3-1001";
        assert_eq!(
            super::windows_private_acl_sddl(current_user, false),
            "D:P(A;;FA;;;S-1-5-21-1-2-3-1001)\
             (A;;FA;;;S-1-5-18)\
             (A;;FA;;;S-1-5-32-544)"
        );
        assert_eq!(
            super::windows_private_acl_sddl(current_user, true),
            "D:P(A;OICI;FA;;;S-1-5-21-1-2-3-1001)\
             (A;OICI;FA;;;S-1-5-18)\
             (A;OICI;FA;;;S-1-5-32-544)"
        );
    }

    #[cfg(windows)]
    #[test]
    fn claude_home_acl_is_left_alone_but_owned_parent_is_hardened() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join(".claude");
        let owned = root.path().join("paper-claude-switch");
        let sibling = root.path().join("paper-claude-switch-old");
        std::fs::create_dir(&shared).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        std::fs::create_dir_all(owned.join("profiles").join("one")).unwrap();

        assert!(!super::should_harden_windows_parent(
            &shared,
            Some(&shared),
            Some(&owned),
            true
        ));
        assert!(super::should_harden_windows_parent(
            &owned.join("profiles").join("one"),
            Some(&owned.join("profiles").join("one")),
            Some(&owned),
            true
        ));
        let owned_alias = owned
            .join("profiles")
            .join("..")
            .join("profiles")
            .join("one");
        assert!(super::should_harden_windows_parent(
            &owned_alias,
            Some(&owned_alias),
            Some(&owned),
            true
        ));
        let sibling_alias = owned.join("..").join("paper-claude-switch-old");
        assert!(!super::should_harden_windows_parent(
            &sibling_alias,
            Some(&sibling_alias),
            Some(&owned),
            true
        ));
        assert!(super::should_harden_windows_parent(
            &shared,
            Some(&shared),
            Some(&shared),
            true
        ));
        assert!(super::should_harden_windows_parent(
            &root.path().join("missing"),
            Some(&root.path().join("missing")),
            Some(&owned),
            false
        ));
        assert!(super::should_harden_windows_parent(
            &shared,
            Some(&shared),
            Some(&root.path().join("missing-owned")),
            true
        ));
    }

    #[cfg(windows)]
    #[test]
    fn protected_temp_is_private_before_first_write_and_failed_create_writes_nothing() {
        use std::os::windows::ffi::OsStrExt;

        fn acl_bytes(path: &Path) -> Vec<Vec<u8>> {
            use std::os::windows::ffi::OsStrExt;

            use windows_sys::Win32::Foundation::LocalFree;
            use windows_sys::Win32::Security::Authorization::{
                GetNamedSecurityInfoW, SE_FILE_OBJECT,
            };
            use windows_sys::Win32::Security::{
                ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce,
            };

            let wide = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut descriptor = std::ptr::null_mut();
            assert_eq!(
                unsafe {
                    GetNamedSecurityInfoW(
                        wide.as_ptr(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut dacl,
                        std::ptr::null_mut(),
                        &mut descriptor,
                    )
                },
                0
            );
            let mut result = Vec::new();
            for index in 0..unsafe { (*dacl).AceCount } as u32 {
                let mut ace = std::ptr::null_mut();
                assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
                let size = unsafe { (*ace.cast::<ACE_HEADER>()).AceSize } as usize;
                result.push(unsafe { std::slice::from_raw_parts(ace.cast::<u8>(), size) }.to_vec());
            }
            unsafe { LocalFree(descriptor) };
            result
        }

        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(windows_system_tool("icacls.exe"))
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)RX"])
            .status()
            .unwrap();
        assert!(status.success(), "failed to seed an extra parent ACE");
        let parent_acl_before = acl_bytes(dir.path());
        let owned_home = dir.path().join("paper-claude-switch");
        std::fs::create_dir(&owned_home).unwrap();
        let temp = super::create_private_windows_temp(dir.path()).unwrap();
        assert_eq!(
            acl_bytes(dir.path()),
            parent_acl_before,
            "temp creation must not rewrite the parent DACL"
        );
        let descriptor = super::windows_acl_security_descriptor(temp.path(), false, false).unwrap();
        let mut dacl = std::ptr::null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        assert_ne!(
            unsafe {
                windows_sys::Win32::Security::GetSecurityDescriptorDacl(
                    descriptor,
                    &mut present,
                    &mut dacl,
                    &mut defaulted,
                )
            },
            0
        );
        let wide = temp
            .path()
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        assert!(super::windows_dacl_already_matches(&wide, dacl));
        assert_eq!(
            temp.as_file().metadata().unwrap().len(),
            0,
            "temp must be private before content is written"
        );

        let shared_path = dir.path().join("auth.json");
        super::atomic_write_private_inner(
            &shared_path,
            b"first-secret",
            Some(dir.path()),
            Some(&owned_home),
        )
        .unwrap();
        assert_eq!(
            acl_bytes(dir.path()),
            parent_acl_before,
            "writing shared auth must not rewrite the shared parent DACL"
        );
        let shared_wide = shared_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        assert!(super::windows_dacl_already_matches(&shared_wide, dacl));
        super::atomic_write_private_inner(
            &shared_path,
            b"replacement-secret",
            Some(dir.path()),
            Some(&owned_home),
        )
        .unwrap();
        assert_eq!(std::fs::read(&shared_path).unwrap(), b"replacement-secret");

        let missing_parent = dir.path().join("does-not-exist");
        let failure = super::create_private_windows_temp(&missing_parent);
        assert!(failure.is_err());
        assert!(std::fs::read_dir(&missing_parent).is_err());
    }

    /// Absolute path of a Windows system tool. Other tests swap `PATH` for a
    /// fake `codex` directory while these run, so a bare name can vanish.
    #[cfg(windows)]
    fn windows_system_tool(relative: &str) -> PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        PathBuf::from(root).join("System32").join(relative)
    }

    #[cfg(windows)]
    #[test]
    fn hardened_windows_dacl_is_recognized_until_an_ace_is_added() {
        use std::os::windows::ffi::OsStrExt;

        let wide = |path: &Path| -> Vec<u16> {
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        };
        let desired = |path: &Path, directory: bool| {
            // Harden once, then read back the DACL the helper compares with.
            super::harden_windows_acl(path, directory).unwrap();
            let mut dacl = std::ptr::null_mut();
            let mut descriptor = std::ptr::null_mut();
            let status = unsafe {
                windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW(
                    wide(path).as_ptr(),
                    windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT,
                    windows_sys::Win32::Security::DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            };
            assert_eq!(status, 0);
            (dacl, descriptor)
        };

        let dir = tempfile::tempdir().unwrap();
        let (dacl, descriptor) = desired(dir.path(), true);
        assert!(
            super::windows_dacl_already_matches(&wide(dir.path()), dacl),
            "a directory hardened a moment ago must not be rewritten"
        );

        let status = std::process::Command::new(windows_system_tool("icacls.exe"))
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .status()
            .unwrap();
        assert!(status.success(), "failed to seed an Everyone ACE");
        assert!(
            !super::windows_dacl_already_matches(&wide(dir.path()), dacl),
            "an extra ACE must force the DACL to be rewritten"
        );
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(descriptor);
        }
    }

    #[cfg(windows)]
    #[test]
    fn atomic_private_write_removes_unknown_explicit_windows_aces() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(windows_system_tool("icacls.exe"))
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .status()
            .unwrap();
        assert!(status.success(), "failed to seed an Everyone ACE");

        let path = dir.path().join("auth.json");
        super::atomic_write_private(&path, br#"{"refresh_token":"secret"}"#).unwrap();
        super::atomic_write_private(&path, br#"{"refresh_token":"replacement"}"#).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            br#"{"refresh_token":"replacement"}"#,
            "a protected temp handle must allow atomic replacement"
        );

        let inspect = r#"
$ErrorActionPreference = 'Stop'
foreach ($item in @($env:CS_ACL_DIR, $env:CS_ACL_FILE)) {
    $acl = if (Test-Path -LiteralPath $item -PathType Container) {
        [IO.Directory]::GetAccessControl($item)
    } else {
        [IO.File]::GetAccessControl($item)
    }
    Write-Output ('protected=' + $acl.AreAccessRulesProtected)
    foreach ($rule in $acl.Access) {
        Write-Output $rule.IdentityReference.Translate(
            [Security.Principal.SecurityIdentifier]
        ).Value
    }
}
"#;
        let output = std::process::Command::new(windows_system_tool(
            r"WindowsPowerShell\v1.0\powershell.exe",
        ))
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            inspect,
        ])
        .env("CS_ACL_DIR", dir.path())
        .env("CS_ACL_FILE", &path)
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "ACL inspection failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let acl = String::from_utf8(output.stdout).unwrap();
        assert_eq!(acl.matches("protected=True").count(), 2);
        assert!(
            !acl.lines().any(|line| line.trim() == "S-1-1-0"),
            "Everyone ACE survived exact DACL replacement:\n{acl}"
        );
    }
}
