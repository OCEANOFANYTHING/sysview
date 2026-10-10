//! Per-platform locations for sysview's mutable state.
//!
//! The dashboard can edit access keys and a few live settings, so those live
//! in a *writable* state directory rather than next to the (often root-owned)
//! binary. Resolution order is deliberately simple and dependency-free:
//!
//! 1. `SYSVIEW_STATE` — explicit override (the systemd unit sets this and
//!    pairs it with `StateDirectory=sysview`, which systemd creates and chowns
//!    to the throwaway service user).
//! 2. `$XDG_STATE_HOME/sysview` (Linux/macOS) or `%LOCALAPPDATA%\sysview`
//!    (Windows).
//! 3. `~/.local/state/sysview` / the temp dir as a last resort.
//!
//! Nothing here is read on the hot path: `serve` resolves these once at start.

use std::path::PathBuf;

/// The writable state directory (created on demand by write helpers).
pub fn state_dir() -> PathBuf {
    if let Some(p) = non_empty(std::env::var("SYSVIEW_STATE").ok()) {
        return PathBuf::from(p);
    }
    if cfg!(windows) {
        if let Some(p) = non_empty(std::env::var("LOCALAPPDATA").ok()) {
            return PathBuf::from(p).join("sysview");
        }
    } else {
        if let Some(p) = non_empty(std::env::var("XDG_STATE_HOME").ok()) {
            return PathBuf::from(p).join("sysview");
        }
        if let Some(home) = non_empty(std::env::var("HOME").ok()) {
            return PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("sysview");
        }
    }
    std::env::temp_dir().join("sysview")
}

/// JSON file holding the dashboard-managed access keys.
pub fn keys_file() -> PathBuf {
    if let Some(p) = non_empty(std::env::var("SYSVIEW_KEYS_FILE").ok()) {
        return PathBuf::from(p);
    }
    state_dir().join("keys.json")
}

/// JSON file holding live-adjustable server settings (interval, kiosk, bind…).
pub fn settings_file() -> PathBuf {
    if let Some(p) = non_empty(std::env::var("SYSVIEW_SETTINGS_FILE").ok()) {
        return PathBuf::from(p);
    }
    state_dir().join("settings.json")
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}
