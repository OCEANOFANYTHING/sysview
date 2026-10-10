//! Access keys: a small, dependency-light credential store.
//!
//! Two sources are merged at validation time:
//!
//! - **Environment** keys (`SYSVIEW_TOKEN`, `SYSVIEW_KEYS`) — immutable at
//!   runtime, the "secure static" set an operator pins in the systemd unit.
//!   They never expire.
//! - **The keys file** (`keys.json` in the state dir) — managed by
//!   `sysview key …` and by the dashboard Settings panel. Each entry carries a
//!   label and an optional expiry.
//!
//! Validation is a constant-time compare against every candidate, so a wrong
//! key cannot be distinguished from a missing one by timing. The file is
//! re-read only when its size/mtime changes (cheap stat + `Arc` swap), which
//! keeps the per-request cost at essentially one `stat` while idle.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One access key. `expires == 0` means "never expires".
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Key {
    pub token: String,
    #[serde(default)]
    pub label: String,
    /// Unix seconds; `0` = never.
    #[serde(default)]
    pub expires: u64,
}

impl Key {
    pub fn expired(&self, now: u64) -> bool {
        self.expires != 0 && self.expires <= now
    }

    /// True when `ident` names this key by exact token or by label.
    pub fn matches_ident(&self, ident: &str) -> bool {
        self.token == ident || (!self.label.is_empty() && self.label == ident)
    }

    pub fn masked(&self) -> String {
        mask(&self.token)
    }
}

/// What a key filesystem entry looked like when we last parsed it, so an
/// unchanged file is not parsed again.
struct Cache {
    mtime: Option<SystemTime>,
    len: u64,
    keys: Arc<Vec<Key>>,
}

pub struct KeyStore {
    file: PathBuf,
    env: Vec<Key>,
    cache: Mutex<Cache>,
}

impl KeyStore {
    /// Builds a store from the environment and the default keys-file path.
    pub fn from_env() -> Self {
        Self::new(crate::paths::keys_file(), env_keys())
    }

    /// Builds a store with an explicit file (used by tests and the CLI).
    pub fn new(file: PathBuf, env: Vec<Key>) -> Self {
        Self {
            file,
            env,
            cache: Mutex::new(Cache {
                mtime: None,
                len: 0,
                keys: Arc::new(Vec::new()),
            }),
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    /// True when at least one key (env or file) can authenticate right now.
    pub fn has_any(&self, now: u64) -> bool {
        self.env.iter().any(|k| !k.expired(now)) || self.file_keys().iter().any(|k| !k.expired(now))
    }

    /// Validates a presented token against every unexpired key.
    pub fn validate(&self, token: &str, now: u64) -> bool {
        if token.is_empty() {
            return false;
        }
        for k in &self.env {
            if !k.expired(now) && ct_eq(token, &k.token) {
                return true;
            }
        }
        for k in self.file_keys().iter() {
            if !k.expired(now) && ct_eq(token, &k.token) {
                return true;
            }
        }
        false
    }

    /// All keys, env first then file (used by `key list` and the settings UI).
    pub fn all(&self) -> Vec<Key> {
        let mut out = self.env.clone();
        out.extend(self.file_keys().iter().cloned());
        out
    }

    /// Keys paired with their source: `true` when from the environment (which
    /// is read-only at runtime and can never expire).
    pub fn entries(&self) -> Vec<(Key, bool)> {
        let mut out: Vec<(Key, bool)> = self.env.iter().cloned().map(|k| (k, true)).collect();
        out.extend(self.file_keys().iter().cloned().map(|k| (k, false)));
        out
    }

    /// Environment keys, in order (read-only at runtime).
    pub fn env_list(&self) -> Vec<Key> {
        self.env.clone()
    }

    /// File keys in stored order (the editable set).
    pub fn file_list(&self) -> Vec<Key> {
        self.file_keys().iter().cloned().collect()
    }

    /// Removes the file key at `idx` (its position in [`KeyStore::file_list`]).
    pub fn remove_index(&self, idx: usize) -> io::Result<bool> {
        let mut keys = (*self.file_keys()).clone();
        if idx >= keys.len() {
            return Ok(false);
        }
        keys.remove(idx);
        self.write(&keys)?;
        Ok(true)
    }

    /// Sets the expiry of the file key at `idx`.
    pub fn set_expiry_index(&self, idx: usize, expires: u64) -> io::Result<bool> {
        let mut keys = (*self.file_keys()).clone();
        if idx >= keys.len() {
            return Ok(false);
        }
        keys[idx].expires = expires;
        self.write(&keys)?;
        Ok(true)
    }

    /// The keys that live in the file only (the editable set).
    pub fn file_keys(&self) -> Arc<Vec<Key>> {
        let meta = fs::metadata(&self.file).ok();
        let mtime = meta.as_ref().and_then(|m| m.modified().ok());
        let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        {
            let c = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            if c.mtime == mtime && c.len == len {
                return Arc::clone(&c.keys);
            }
        }
        let parsed = Arc::new(read_keys(&self.file).unwrap_or_default());
        let mut c = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        c.mtime = mtime;
        c.len = len;
        c.keys = Arc::clone(&parsed);
        parsed
    }

    /// Adds (or replaces, by label) a file key.
    pub fn add(&self, key: Key) -> io::Result<()> {
        let mut keys = (*self.file_keys()).clone();
        keys.retain(|k| !(k.token == key.token || (!key.label.is_empty() && k.label == key.label)));
        keys.push(key);
        self.write(&keys)
    }

    /// Removes a file key by token or label. Returns whether anything matched.
    pub fn remove(&self, ident: &str) -> io::Result<bool> {
        let mut keys = (*self.file_keys()).clone();
        let before = keys.len();
        keys.retain(|k| !k.matches_ident(ident));
        if keys.len() == before {
            return Ok(false);
        }
        self.write(&keys)?;
        Ok(true)
    }

    /// Sets the expiry on a file key by token or label.
    pub fn set_expiry(&self, ident: &str, expires: u64) -> io::Result<bool> {
        let mut keys = (*self.file_keys()).clone();
        let mut hit = false;
        for k in keys.iter_mut() {
            if k.matches_ident(ident) {
                k.expires = expires;
                hit = true;
            }
        }
        if !hit {
            return Ok(false);
        }
        self.write(&keys)?;
        Ok(true)
    }

    /// Removes every file key except `keep_token` (the "rotate" helper).
    pub fn keep_only(&self, keep_token: &str) -> io::Result<()> {
        let mut keys = (*self.file_keys()).clone();
        keys.retain(|k| k.token == keep_token);
        self.write(&keys)
    }

    fn write(&self, keys: &[Key]) -> io::Result<()> {
        write_keys(&self.file, keys)
    }
}

/// Reads and parses a keys file, tolerating an absent file (empty set).
pub fn read_keys(path: &Path) -> io::Result<Vec<Key>> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(Vec::new())
            } else {
                serde_json::from_str(trimmed)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Writes a keys file atomically (temp + rename), creating the dir on demand.
pub fn write_keys(path: &Path, keys: &[Key]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(keys)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;
    #[cfg(windows)]
    {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path)
}

/// Reads the environment key sources.
pub fn env_keys() -> Vec<Key> {
    let mut out = Vec::new();
    if let Some(t) = std::env::var("SYSVIEW_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
    {
        out.push(Key {
            token: t,
            label: "env".into(),
            expires: 0,
        });
    }
    if let Ok(list) = std::env::var("SYSVIEW_KEYS") {
        for entry in list.split([',', ';']) {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let parts: Vec<&str> = entry.split(':').collect();
            let key = match parts.as_slice() {
                [token] => Key {
                    token: (*token).into(),
                    label: String::new(),
                    expires: 0,
                },
                [label, token] => Key {
                    token: (*token).into(),
                    label: (*label).into(),
                    expires: 0,
                },
                [label, token, exp] => Key {
                    token: (*token).into(),
                    label: (*label).into(),
                    expires: exp.parse().unwrap_or(0),
                },
                _ => continue,
            };
            if !key.token.is_empty() {
                out.push(key);
            }
        }
    }
    out
}

/// A short, non-reversible-looking display form: first 4 + last 4 characters.
pub fn mask(token: &str) -> String {
    let n = token.chars().count();
    if n <= 8 {
        return "\u{2022}\u{2022}\u{2022}\u{2022}".into();
    }
    let head: String = token.chars().take(4).collect();
    let tail: String = token.chars().skip(n - 4).collect();
    format!("{head}\u{2026}{tail}")
}

/// Parses an expiry specification into a Unix timestamp, or `0` for "never".
///
/// Accepts `never`/`infinity`/`0`, bare or unit-suffixed durations
/// (`30m`, `12h`, `7d`, `2w`, `6mo`, `1y`), a bare number of days, an explicit
/// epoch (values >= 1e9), and an ISO date `YYYY-MM-DD`.
pub fn parse_expiry(spec: &str, now: u64) -> Result<u64, String> {
    let s = spec.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err("empty expiry".into());
    }
    match s.as_str() {
        "never" | "forever" | "infinity" | "inf" | "none" | "0" => return Ok(0),
        _ => {}
    }
    // ISO date YYYY-MM-DD
    if s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        let y: i64 = s[0..4].parse().map_err(|_| format!("bad date '{spec}'"))?;
        let m: u32 = s[5..7].parse().map_err(|_| format!("bad date '{spec}'"))?;
        let d: u32 = s[8..10].parse().map_err(|_| format!("bad date '{spec}'"))?;
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return Err(format!("bad date '{spec}'"));
        }
        let days = days_from_civil(y, m, d);
        return Ok((days * 86_400).max(0) as u64);
    }
    // number + optional unit
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_s, unit) = s.split_at(split);
    let unit = unit.trim();
    let num: f64 = num_s.parse().map_err(|_| format!("bad expiry '{spec}'"))?;
    if unit.is_empty() {
        // Bare number: days, unless it looks like an epoch.
        if num >= 1_000_000_000.0 {
            return Ok(num as u64);
        }
        return Ok(now.saturating_add((num * 86_400.0) as u64));
    }
    let unit_secs: f64 = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600.0,
        "d" | "day" | "days" => 86_400.0,
        "w" | "week" | "weeks" => 604_800.0,
        "mo" | "month" | "months" => 2_592_000.0, // 30 days
        "y" | "yr" | "yrs" | "year" | "years" => 31_536_000.0, // 365 days
        _ => return Err(format!("unknown unit in '{spec}'")),
    };
    Ok(now.saturating_add((num * unit_secs) as u64))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Current Unix time in seconds (`0` if the clock is before the epoch).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Formats a Unix timestamp as `YYYY-MM-DD HH:MM:SS UTC`.
pub fn format_time(epoch: u64) -> String {
    let secs = epoch as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} UTC")
}

/// Inverse of [`days_from_civil`] (Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Generates a random hex token (32 chars). Uses the OS CSPRNG where
/// available (`/dev/urandom`) and a SHA-256-mixed fallback elsewhere, so no
/// random-number dependency is needed.
pub fn generate_token() -> String {
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut seed = [0u8; 32];
        if let Ok(mut f) = fs::File::open("/dev/urandom")
            && f.read_exact(&mut seed).is_ok()
        {
            return hex(&seed[..16]);
        }
    }
    // Fallback: hash several entropy sources together.
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(now_unix());
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    let stack = 0u8;
    h.write_usize(&stack as *const u8 as usize);
    let v = h.finish();
    use sha2::{Digest, Sha256};
    let mut sha = Sha256::new();
    sha.update(v.to_le_bytes());
    sha.update(now_unix().to_le_bytes());
    sha.update(std::process::id().to_le_bytes());
    let out = sha.finalize();
    hex(&out[..16])
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Constant-time byte comparison (resists timing attacks on tokens).
pub fn ct_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..ab.len() {
        diff |= ab[i] ^ bb[i];
    }
    diff == 0
}

impl Key {
    /// Test-only convenience constructor (keeps fixtures terse).
    #[cfg(test)]
    fn from(token: &str, label: &str, expires: u64) -> Self {
        Key {
            token: token.into(),
            label: label.into(),
            expires,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sysview-keys-{tag}-{}.json", std::process::id()));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn validate_respects_expiry() {
        let path = tmp_path("exp");
        let store = KeyStore::new(path.clone(), Vec::new());
        store
            .add(Key {
                token: "live".into(),
                label: "live".into(),
                expires: 0,
            })
            .unwrap();
        store
            .add(Key {
                token: "soon".into(),
                label: "soon".into(),
                expires: 100,
            })
            .unwrap();
        assert!(store.validate("live", 500));
        assert!(store.validate("soon", 50));
        assert!(!store.validate("soon", 100)); // expires at the boundary
        assert!(!store.validate("nope", 50));
        assert!(!store.validate("", 50));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn add_remove_and_expiry_roundtrip() {
        let path = tmp_path("round");
        let store = KeyStore::new(path.clone(), Vec::new());
        store.add(Key::from("aaa", "phone", 0)).unwrap();
        store.add(Key::from("bbb", "laptop", 0)).unwrap();
        assert_eq!(store.file_keys().len(), 2);
        assert!(store.set_expiry("phone", 42).unwrap());
        assert_eq!(store.file_keys()[0].expires, 42);
        assert!(store.remove("laptop").unwrap());
        assert_eq!(store.file_keys().len(), 1);
        assert!(!store.remove("ghost").unwrap());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn env_keys_are_merged_and_never_expire() {
        let path = tmp_path("env");
        let store = KeyStore::new(
            path.clone(),
            vec![Key {
                token: "envtok".into(),
                label: "env".into(),
                expires: 0,
            }],
        );
        store.add(Key::from("filetok", "f", 1)).unwrap();
        assert!(store.validate("envtok", u64::MAX));
        assert!(store.validate("filetok", 0));
        assert!(!store.validate("filetok", 2));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn expiry_specs_parse() {
        let now = 1_000_000;
        assert_eq!(parse_expiry("never", now).unwrap(), 0);
        assert_eq!(parse_expiry("0", now).unwrap(), 0);
        assert_eq!(parse_expiry("1d", now).unwrap(), now + 86_400);
        assert_eq!(parse_expiry("7 days", now).unwrap(), now + 7 * 86_400);
        assert_eq!(parse_expiry("30d", now).unwrap(), now + 30 * 86_400);
        assert_eq!(parse_expiry("6mo", now).unwrap(), now + 6 * 2_592_000);
        assert_eq!(parse_expiry("1y", now).unwrap(), now + 31_536_000);
        assert_eq!(parse_expiry("12h", now).unwrap(), now + 43_200);
        assert_eq!(parse_expiry("30", now).unwrap(), now + 30 * 86_400);
        // 2026-10-09 00:00:00 UTC
        assert_eq!(parse_expiry("2026-10-09", now).unwrap(), 1_791_504_000);
        assert!(parse_expiry("soon", now).is_err());
    }

    #[test]
    fn generated_tokens_are_distinct_and_hex() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn mask_hides_the_middle() {
        assert_eq!(mask("1234567890abcdef"), "1234\u{2026}cdef");
        assert_eq!(mask("short"), "\u{2022}\u{2022}\u{2022}\u{2022}");
    }

    #[test]
    fn format_time_roundtrips_known_epochs() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_time(1_791_504_000), "2026-10-09 00:00:00 UTC");
        // parse_expiry of a date should format back to that date.
        assert_eq!(
            parse_expiry("2026-10-09", 0).map(format_time),
            Ok("2026-10-09 00:00:00 UTC".to_string())
        );
    }
}
