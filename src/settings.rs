//! Dashboard-adjustable settings, persisted between restarts.
//!
//! These are *overrides* the operator changed from the web UI. Precedence at
//! startup is: explicit CLI flag (except interval, see below) > this file >
//! built-in default. Every field is optional, so the file stays tiny and only
//! ever holds what actually differs from the defaults.
//!
//! The file lives in the writable state dir (see [`crate::paths`]). It is read
//! once at startup and rewritten only when the settings page saves; nothing
//! here touches the request hot path.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Persisted overrides. `None` means "not customised".
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Bind address, e.g. `0.0.0.0` or `192.168.100.114`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// Interface name to bind to (resolved to its IPv4 at startup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// TCP port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Sampling/poll interval in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<f64>,
    /// Max processes sent to the dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_procs: Option<usize>,
    /// Whether new viewers start in kiosk mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kiosk: Option<bool>,
}

impl Settings {
    /// Loads settings, treating a missing or malformed file as "all defaults"
    /// (malformed is tolerated so a bad edit can never stop the daemon booting).
    pub fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_default(),
            _ => Settings::default(),
        }
    }

    /// Writes settings atomically (temp + rename), creating the dir on demand.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &json)?;
        #[cfg(windows)]
        {
            let _ = fs::remove_file(path);
        }
        fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_defaults() {
        let p = std::env::temp_dir().join("sysview-settings-missing.json");
        let _ = fs::remove_file(&p);
        let s = Settings::load(&p);
        assert!(s.interval_secs.is_none() && s.bind.is_none());
    }

    #[test]
    fn roundtrip_persists_only_set_fields() {
        let p = std::env::temp_dir().join(format!("sysview-settings-{}.json", std::process::id()));
        let _ = fs::remove_file(&p);
        let s = Settings {
            interval_secs: Some(2.5),
            max_procs: Some(42),
            kiosk: Some(true),
            ..Default::default()
        };
        s.save(&p).unwrap();
        let raw = fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("\"bind\"")); // unset fields are not serialized
        let back = Settings::load(&p);
        assert_eq!(back.interval_secs, Some(2.5));
        assert_eq!(back.max_procs, Some(42));
        assert_eq!(back.kiosk, Some(true));
        let _ = fs::remove_file(&p);
    }
}
