//! Local state: connection profiles (the equivalent of Xshell's session list),
//! trusted host keys, and the rules that decide where files go.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::{t, tf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Profile {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    /// Private key: either a path to a key file, or the key itself pasted in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Older configs stored only a path; still read, never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
    /// Whether a password exists in the encrypted secrets blob.  Set
    /// automatically.
    #[serde(default)]
    pub has_secret: bool,
    /// Remote directory uploads go to.  Empty means the session's shell
    /// directory — where the ssh prompt has `cd`-ed to — falling back to the
    /// remote home directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_dir: Option<String>,
    /// Local directory downloads land in.  Empty means the command bar's
    /// current directory, which starts at the desktop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<String>,
    #[serde(default)]
    pub note: String,
}

fn default_port() -> u16 {
    22
}

/// A cheap random source: enough to pick a label, and no extra dependency.
fn next_rand() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut s = STATE.load(Ordering::Relaxed);
    if s == 0 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        s = nanos ^ ((std::process::id() as u64) << 32) | 1;
    }
    // xorshift64*
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    STATE.store(s, Ordering::Relaxed);
    s.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// A label for a new connection: one letter and two digits, e.g. `k07`.
///
/// Sessions are numbered with digits only, so a connection label can never be
/// mistaken for a running session, and the caller's existing names are avoided
/// so two connections never share a label.
pub fn make_profile_name(taken: impl Iterator<Item = String>) -> String {
    let taken: Vec<String> = taken.collect();
    for _ in 0..512 {
        let r = next_rand();
        let letter = (b'a' + (r % 26) as u8) as char;
        let n = (r >> 8) % 100;
        let name = format!("{letter}{n:02}");
        if !taken.iter().any(|t| t == &name) {
            return name;
        }
    }
    // A deterministic sweep, in case we are absurdly unlucky.
    for a in b'a'..=b'z' {
        for n in 0..100u32 {
            let name = format!("{}{:02}", a as char, n);
            if !taken.iter().any(|t| t == &name) {
                return name;
            }
        }
    }
    "zz99".to_string()
}

impl Profile {
    pub fn new(name: &str, host: &str, user: &str) -> Profile {
        Profile {
            name: name.to_string(),
            host: host.to_string(),
            port: 22,
            user: user.to_string(),
            ..Default::default()
        }
    }

    pub fn addr(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }

    /// The key the user configured, however they configured it.
    pub fn key_source(&self) -> Option<&str> {
        self.key
            .as_deref()
            .or(self.key_path.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// True when the key field holds the key itself rather than a path.
    pub fn key_is_inline(&self) -> bool {
        self.key_source()
            .map(|s| s.starts_with("-----BEGIN") || s.contains("PRIVATE KEY-----"))
            .unwrap_or(false)
    }

    /// Removes empty optional strings so the saved json stays readable.
    pub fn tidy(&mut self) {
        for s in [&mut self.upload_dir, &mut self.download_dir, &mut self.key] {
            if s.as_ref().map(|v| v.trim().is_empty()).unwrap_or(false) {
                *s = None;
            }
        }
        self.name = self.name.trim().to_string();
        self.host = self.host.trim().to_string();
        self.user = self.user.trim().to_string();
        // Fold the legacy path field into the one the ui edits.
        if self.key.is_none() {
            self.key = self.key_path.take();
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err(t!("name cannot be empty").into());
        }
        if self.name.contains('/') || self.name.contains('\\') {
            return Err(t!("name cannot contain a slash").into());
        }
        if self.host.trim().is_empty() {
            return Err(t!("host cannot be empty").into());
        }
        if self.user.trim().is_empty() {
            return Err(t!("user cannot be empty").into());
        }
        if self.port == 0 {
            return Err(t!("port must be 1-65535").into());
        }
        Ok(())
    }
}

/// A cheap, shared handle to the client's local state.
///
/// Every clone points at the same behind-the-scenes data, so the ui and the
/// controller tasks all run against one set of profiles, known hosts and
/// secrets.  Every call is synchronous — the mutex is never held across an
/// `.await` — and writes go through a temporary file so a crash or a concurrent
/// writer can never leave a half-written file behind.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    dir: PathBuf,
    data: Data,
    secrets: BTreeMap<String, String>,
    /// Set when `sessions.json` could not be parsed and had to be set aside, so
    /// the caller can tell the user instead of silently starting empty.
    load_warning: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Data {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// host:port -> "SHA256:..." fingerprint of the accepted key.  Entries from
    /// before the port was part of the key stay readable as bare-host fallbacks.
    #[serde(default)]
    pub known_hosts: BTreeMap<String, String>,
    /// `auto`, `en` or `zh`.  Absent means "follow the system", which is the
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

/// Where the client keeps its state: a `fabssh` folder alongside the user's
/// Documents, overridable with `TABSSH_CONFIG_DIR` (used by the test suite).
/// This is a Windows program, so there is deliberately no XDG/Linux fallback.
pub fn config_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("TABSSH_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    documents_dir().join("fabssh")
}

/// Where the old build kept its state (`%APPDATA%\tabssh`).  Read once, so an
/// upgrade does not lose saved connections.
fn legacy_config_dir() -> Option<PathBuf> {
    let a = std::env::var_os("APPDATA")?;
    if a.is_empty() {
        return None;
    }
    Some(PathBuf::from(a).join("tabssh"))
}

pub fn home_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(p);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The user's Documents folder — where the `fabssh` state folder now lives.
///
/// The plain `Documents` name is tried first, then the OneDrive redirects and
/// the localised Chinese name, the same way [`desktop_dir`] finds the desktop.
pub fn documents_dir() -> PathBuf {
    let home = home_dir();
    for candidate in ["Documents", "OneDrive/Documents", "OneDrive/文档", "文档"] {
        let p = home.join(candidate);
        if p.is_dir() {
            return p;
        }
    }
    home
}

/// The desktop, which is where local browsing and downloads start.
pub fn desktop_dir() -> PathBuf {
    let home = home_dir();
    for candidate in ["Desktop", "OneDrive/Desktop", "OneDrive/桌面", "桌面"] {
        let p = home.join(candidate);
        if p.is_dir() {
            return p;
        }
    }
    home
}

/// Where downloads go when nothing else is configured: the desktop, which is
/// where people look for a file they just pulled down.
pub fn default_download_dir() -> PathBuf {
    desktop_dir()
}

/// The client's own download folder — the last-resort fallback.
pub fn download_dir() -> PathBuf {
    let d = config_dir().join("downloads");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// The folder the settings form shows for a connection's downloads: the
/// directory set on it, or the desktop when it is blank.
///
/// This is only the form's display fallback.  A real `get` goes to the F2
/// command bar's current directory (which itself starts at the desktop) unless
/// the connection names a directory — see `ui::command::download_target`.
pub fn download_dir_for(profile: &Profile) -> PathBuf {
    if let Some(custom) = profile
        .download_dir
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let p = PathBuf::from(custom);
        if std::fs::create_dir_all(&p).is_ok() {
            return p;
        }
        // Otherwise fall through to the defaults below.
    }
    let d = default_download_dir();
    if std::fs::create_dir_all(&d).is_ok() {
        d
    } else {
        download_dir()
    }
}

impl Store {
    /// Opens the shared store, loading `sessions.json` and the encrypted
    /// secrets blob.
    ///
    /// A `sessions.json` that does not parse is moved aside as
    /// `sessions.json.corrupt-<unix-secs>` and empty settings are used; the
    /// warning is kept so the caller can tell the user.
    ///
    /// The first time it runs in the new location it moves state across from
    /// the old `%APPDATA%\tabssh` folder, so an upgrade loses nothing.
    pub fn open() -> Store {
        let dir = config_dir();
        let sessions = dir.join("sessions.json");
        let mut data = Data::default();
        let mut load_warning = None;
        match std::fs::read_to_string(&sessions) {
            Ok(text) => match serde_json::from_str::<Data>(&text) {
                Ok(parsed) => data = parsed,
                Err(_) => {
                    let secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let backup = dir.join(format!("sessions.json.corrupt-{secs}"));
                    let _ = std::fs::rename(&sessions, &backup);
                    load_warning = Some(tf!(
                        "could not read {} — kept it as {} and started empty",
                        sessions.display(),
                        backup.display()
                    ));
                }
            },
            Err(_) => {}
        }
        let (mut secrets, secrets_warning) = load_secrets(&dir);
        if load_warning.is_none() {
            load_warning = secrets_warning;
        }
        let migrated = migrate_legacy(&dir, &mut data, &mut secrets);
        let store = Store {
            inner: Arc::new(Mutex::new(Inner {
                dir,
                data,
                secrets,
                load_warning,
            })),
        };
        if migrated {
            // Persist the moved state in the new, encrypted location.
            let _ = store.save();
            let _ = store.save_secrets();
        }
        store
    }

    pub fn save(&self) -> std::io::Result<()> {
        let (dir, json) = {
            let mut inner = self.inner.lock().unwrap();
            inner.data.schema_version = 1;
            (
                inner.dir.clone(),
                serde_json::to_string_pretty(&inner.data).unwrap_or_default(),
            )
        };
        atomic_write(&dir.join("sessions.json"), &json)
    }

    pub fn save_secrets(&self) -> std::io::Result<()> {
        let (dir, json) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.dir.clone(),
                serde_json::to_vec(&inner.secrets).unwrap_or_default(),
            )
        };
        // Passwords never reach the disk in the clear: the json map is wrapped
        // in DPAPI, which only this Windows user (on this machine) can unwrap.
        let blob = protect(&json)?;
        let p = dir.join(SECRETS_FILE);
        atomic_write_bytes(&p, &blob)?;
        restrict(&p);
        Ok(())
    }

    pub fn profiles(&self) -> Vec<Profile> {
        self.inner.lock().unwrap().data.profiles.clone()
    }

    pub fn find(&self, name: &str) -> Option<Profile> {
        self.inner
            .lock()
            .unwrap()
            .data
            .profiles
            .iter()
            .find(|p| p.name == name)
            .cloned()
    }

    pub fn lang(&self) -> Option<String> {
        self.inner.lock().unwrap().data.lang.clone()
    }

    pub fn known_hosts(&self) -> BTreeMap<String, String> {
        self.inner.lock().unwrap().data.known_hosts.clone()
    }

    pub fn secret(&self, name: &str) -> Option<String> {
        self.inner.lock().unwrap().secrets.get(name).cloned()
    }

    pub fn take_load_warning(&self) -> Option<String> {
        self.inner.lock().unwrap().load_warning.take()
    }

    pub fn upsert(&self, p: Profile) {
        let mut inner = self.inner.lock().unwrap();
        match inner.data.profiles.iter_mut().find(|x| x.name == p.name) {
            Some(slot) => *slot = p,
            None => inner.data.profiles.push(p),
        }
    }

    pub fn remove(&self, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.data.profiles.retain(|p| p.name != name);
        inner.secrets.remove(name);
    }

    pub fn set_lang(&self, lang: Option<String>) {
        self.inner.lock().unwrap().data.lang = lang;
    }

    pub fn set_secret(&self, name: &str, pw: String) {
        self.inner
            .lock()
            .unwrap()
            .secrets
            .insert(name.to_string(), pw);
    }

    pub fn remove_secret(&self, name: &str) {
        self.inner.lock().unwrap().secrets.remove(name);
    }

    pub fn remember_host(&self, key: String, fp: String) {
        self.inner
            .lock()
            .unwrap()
            .data
            .known_hosts
            .insert(key, fp);
    }
}

impl Default for Store {
    fn default() -> Store {
        Store {
            inner: Arc::new(Mutex::new(Inner {
                dir: config_dir(),
                data: Data::default(),
                secrets: BTreeMap::new(),
                load_warning: None,
            })),
        }
    }
}

/// File holding the DPAPI-wrapped password map.
const SECRETS_FILE: &str = "secrets.bin";

/// Writes `contents` to `path` through a sibling temporary file, so a crash or
/// a concurrent writer can never leave a half-written file behind.
///
/// The temporary name is unique per call (a process-wide counter plus the pid),
/// so two overlapping `save()` calls — the ui thread and a controller task can
/// both write `sessions.json` — never share, and therefore never truncate, the
/// same temp file.
fn atomic_write(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    atomic_write_bytes(path, contents.as_bytes())
}

/// The byte form of [`atomic_write`]; `contents` may be binary (the encrypted
/// secrets blob).  The temp file is flushed to disk before the rename, so a
/// power loss cannot leave an empty file where a full one was expected.
fn atomic_write_bytes(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(tmp);
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(contents)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// Tightening file permissions is a unix idea; on Windows the files live in the
/// user's own `Documents\fabssh`, and the passwords are encrypted besides, so
/// there is nothing to do.  Kept as a hook so the call sites read the same.
pub fn restrict(path: &std::path::Path) {
    let _ = path;
}

/// Loads the password map from `dir`.
///
/// The current format is a DPAPI-encrypted blob ([`SECRETS_FILE`]); a plaintext
/// `secrets.json` left behind by an older build is still read — it is simply
/// written back encrypted the next time secrets are saved.
///
/// A blob that exists but cannot be decrypted or parsed is moved aside as
/// `secrets.bin.corrupt-<unix-secs>` rather than silently forgotten: the next
/// `save_secrets()` would otherwise overwrite it with an empty map and the
/// passwords would be unrecoverable.  The warning text is returned for the
/// caller to show.
fn load_secrets(dir: &std::path::Path) -> (BTreeMap<String, String>, Option<String>) {
    let blob_path = dir.join(SECRETS_FILE);
    if let Ok(blob) = std::fs::read(&blob_path) {
        let decoded = unprotect(&blob)
            .ok()
            .and_then(|json| serde_json::from_slice::<BTreeMap<String, String>>(&json).ok());
        match decoded {
            Some(map) => return (map, None),
            None => {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup = dir.join(format!("{}.corrupt-{secs}", SECRETS_FILE));
                let _ = std::fs::rename(&blob_path, &backup);
                return (
                    BTreeMap::new(),
                    Some(tf!(
                        "could not read {} — kept it as {} and started empty",
                        blob_path.display(),
                        backup.display()
                    )),
                );
            }
        }
    }
    let legacy = std::fs::read_to_string(dir.join("secrets.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    (legacy, None)
}

/// Moves state across from the old `%APPDATA%\tabssh` folder, once.
///
/// Only runs in a real run (never under the test override) and only while the
/// new location has nothing of its own, so it can never clobber newer state.
/// Returns true when something was actually taken over.
fn migrate_legacy(
    dir: &std::path::Path,
    data: &mut Data,
    secrets: &mut BTreeMap<String, String>,
) -> bool {
    if std::env::var_os("TABSSH_CONFIG_DIR").is_some() {
        return false;
    }
    let Some(old) = legacy_config_dir() else {
        return false;
    };
    if old == dir {
        return false;
    }
    // Only migrate into an empty new location.
    if dir.join("sessions.json").exists() || dir.join(SECRETS_FILE).exists() || !secrets.is_empty()
    {
        return false;
    }
    let mut changed = false;
    if let Ok(text) = std::fs::read_to_string(old.join("sessions.json")) {
        if let Ok(parsed) = serde_json::from_str::<Data>(&text) {
            if !parsed.profiles.is_empty() || !parsed.known_hosts.is_empty() {
                *data = parsed;
                changed = true;
            }
        }
    }
    // The old passwords were plaintext json; load them so they are written back
    // encrypted.  The old file is left in place — deleting user data is not
    // ours to do.
    if let Ok(text) = std::fs::read_to_string(old.join("secrets.json")) {
        if let Ok(map) = serde_json::from_str::<BTreeMap<String, String>>(&text) {
            if !map.is_empty() {
                *secrets = map;
                changed = true;
            }
        }
    }
    changed
}

/// Wraps `plain` in DPAPI, bound to the current Windows user.
///
/// The encryption key is derived from the user's login, so the blob is useless
/// copied to another account or machine and no key has to be managed.
fn protect(plain: &[u8]) -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
    };
    unsafe {
        let data_in = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut data_out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = CryptProtectData(
            &data_in,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut data_out,
        );
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let out = std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize).to_vec();
        // CryptProtectData allocates with LocalAlloc; free it the same way.
        windows_sys::Win32::Foundation::LocalFree(data_out.pbData as *mut core::ffi::c_void);
        Ok(out)
    }
}

/// Reverses [`protect`].
fn unprotect(blob: &[u8]) -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
    };
    unsafe {
        let data_in = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut data_out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let mut descr: windows_sys::core::PWSTR = std::ptr::null_mut();
        let ok = CryptUnprotectData(
            &data_in,
            &mut descr,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut data_out,
        );
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let out = std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize).to_vec();
        if !descr.is_null() {
            windows_sys::Win32::Foundation::LocalFree(descr as *mut core::ffi::c_void);
        }
        windows_sys::Win32::Foundation::LocalFree(data_out.pbData as *mut core::ffi::c_void);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests share one process, so the config directory has to be pinned before
    /// any test reads it.  Both test modules set the same value.
    fn isolate_config() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let dir = std::env::temp_dir().join("tabssh-test-config");
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("TABSSH_CONFIG_DIR", &dir);
        });
    }

    #[test]
    fn roundtrip_store() {
        let s = Store::default();
        let mut p = Profile::new("web", "example.com", "root");
        p.port = 2222;
        p.upload_dir = Some("/srv/web".into());
        s.upsert(p.clone());
        s.upsert(p.clone()); // upsert must not duplicate
        assert_eq!(s.profiles().len(), 1);
        let json = serde_json::to_string(&s.inner.lock().unwrap().data).unwrap();
        let back: Data = serde_json::from_str(&json).unwrap();
        assert_eq!(back.profiles[0].port, 2222);
        assert_eq!(back.profiles[0].upload_dir.as_deref(), Some("/srv/web"));
        assert_eq!(back.profiles[0].addr(), "root@example.com:2222");
    }

    #[test]
    fn connection_labels_are_a_letter_and_two_digits() {
        for _ in 0..50 {
            let name = make_profile_name(std::iter::empty());
            assert_eq!(name.len(), 3, "{name}");
            let mut chars = name.chars();
            assert!(chars.next().unwrap().is_ascii_lowercase(), "{name}");
            assert!(chars.all(|c| c.is_ascii_digit()), "{name}");
        }
    }

    #[test]
    fn connection_labels_step_around_the_ones_already_taken() {
        let taken: Vec<String> = (0..100).map(|n| format!("a{n:02}")).collect();
        for _ in 0..100 {
            let name = make_profile_name(taken.iter().cloned());
            assert!(!taken.contains(&name), "reused {name}");
        }
    }

    #[test]
    fn defaults_are_conservative() {
        let p = Profile::new("n", "h", "u");
        assert!(p.upload_dir.is_none());
        assert!(p.download_dir.is_none());
    }

    #[test]
    fn an_older_config_with_the_removed_flags_still_loads() {
        // The `screen` and `persistent` keys are gone from the schema; a file
        // written by an older build still parses, with the unknown keys simply
        // ignored.
        let json = r#"{"profiles":[{"name":"a01","host":"h","user":"u","screen":true,"persistent":true}],
                       "history":[],"known_hosts":{}}"#;
        let data: Data = serde_json::from_str(json).unwrap();
        let p = &data.profiles[0];
        assert_eq!(p.name, "a01");
        assert_eq!(p.host, "h");
        assert!(p.upload_dir.is_none());
    }

    #[test]
    fn tidy_clears_blank_optional_dirs() {
        let mut p = Profile::new("n", "h", "u");
        p.upload_dir = Some("   ".into());
        p.download_dir = Some(String::new());
        p.tidy();
        assert!(p.upload_dir.is_none());
        assert!(p.download_dir.is_none());
    }

    #[test]
    fn validation_catches_the_obvious_mistakes() {
        let mut p = Profile::new("", "h", "u");
        assert!(p.validate().is_err());
        p.name = "a/b".into();
        assert!(p.validate().is_err());
        p.name = "ok".into();
        p.host = String::new();
        assert!(p.validate().is_err());
        p.host = "h".into();
        p.port = 0;
        assert!(p.validate().is_err());
        p.port = 22;
        assert!(p.validate().is_ok());
    }

    #[test]
    fn the_default_download_dir_is_the_desktop() {
        let p = Profile::new("n", "h", "u");
        assert_eq!(download_dir_for(&p), desktop_dir());
    }

    #[test]
    fn download_dir_prefers_the_profile_then_defaults() {
        isolate_config();
        let dir = std::env::temp_dir().join("tabssh-dl-test");
        let mut p = Profile::new("n", "h", "u");
        p.download_dir = Some(dir.display().to_string());
        assert_eq!(download_dir_for(&p), dir);

        // A blank setting falls back to the desktop.
        p.download_dir = Some("  ".into());
        assert_eq!(download_dir_for(&p), default_download_dir());
        assert!(download_dir_for(&p).is_dir());
    }

    #[test]
    fn secrets_survive_an_encrypt_decrypt_roundtrip() {
        let map = BTreeMap::from([
            ("a01".to_string(), "s3cret".to_string()),
            ("b02".to_string(), "pa55 w/ spaces".to_string()),
        ]);
        let json = serde_json::to_vec(&map).unwrap();
        let blob = protect(&json).unwrap();
        // The disk form must not contain the plaintext.
        assert!(!blob.windows(6).any(|w| w == b"s3cret"));
        let back: BTreeMap<String, String> = serde_json::from_slice(&unprotect(&blob).unwrap()).unwrap();
        assert_eq!(back, map);
    }

    #[test]
    fn unprotect_rejects_a_foreign_blob() {
        // A blob that is not DPAPI output must be a clean error, not a panic.
        assert!(unprotect(b"not a dpapi blob").is_err());
    }

    #[test]
    fn a_plaintext_secrets_file_is_still_read() {
        // The old format: a plain json map next to where the new blob goes.
        let dir = std::env::temp_dir().join("tabssh-secrets-legacy");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("secrets.json"),
            r#"{"old01":"hunter2"}"#,
        )
        .unwrap();
        let (map, warning) = load_secrets(&dir);
        assert!(warning.is_none());
        assert_eq!(map.get("old01").map(String::as_str), Some("hunter2"));
    }

    #[test]
    fn an_unreadable_secrets_blob_is_kept_aside_not_wiped() {
        // A blob that will not decrypt must survive: overwriting it with an
        // empty map on the next save would lose the passwords for good.
        let dir = std::env::temp_dir().join("tabssh-secrets-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SECRETS_FILE), b"not a dpapi blob").unwrap();
        let (map, warning) = load_secrets(&dir);
        assert!(map.is_empty());
        assert!(warning.is_some(), "the user must hear about it");
        assert!(
            !dir.join(SECRETS_FILE).exists(),
            "moved aside, so a save cannot overwrite it"
        );
        let kept: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("secrets.bin.corrupt-"))
            .collect();
        assert_eq!(kept.len(), 1, "the blob is preserved for recovery");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_secrets_are_encrypted_on_disk_and_reload() {
        isolate_config();
        let s = Store::default();
        s.set_secret("a01", "topsecret".into());
        s.save_secrets().unwrap();

        // The bytes on disk must not contain the password.
        let raw = std::fs::read(config_dir().join(SECRETS_FILE)).unwrap();
        assert!(
            !raw.windows(9).any(|w| w == b"topsecret"),
            "the password was written in the clear"
        );
        // And it must come back intact.
        let (back, warning) = load_secrets(&config_dir());
        assert!(warning.is_none());
        assert_eq!(back.get("a01").map(String::as_str), Some("topsecret"));
    }

    #[test]
    fn documents_dir_is_a_real_folder() {
        // The state folder hangs off a real Documents (or its OneDrive /
        // localised spelling), falling back to the home directory.  No env is
        // touched, so this cannot race the tests that pin the config dir.
        let docs = documents_dir();
        assert!(docs.is_dir(), "{}", docs.display());
        assert!(docs.starts_with(home_dir()), "{}", docs.display());
    }
}
