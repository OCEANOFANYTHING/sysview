//! Self-update from GitHub Releases — no external tools (no `git`, no `curl`).
//!
//! A tiny bundled HTTPS client (`minreq` + rustls) fetches the latest release
//! metadata and the matching prebuilt binary, verifies it against the release's
//! `SHA256SUMS`, and swaps it into place in the same directory as the running
//! executable. The **token and configuration are never touched**, so an
//! existing dashboard URL keeps working after an update.
//!
//! Cost model: nothing here runs during normal serving. The whole module is
//! only exercised by `sysview update` and the dashboard's Update button, both
//! short-lived, so the resident server's memory is unchanged.
//!
//! Platform replacement strategy:
//! - Unix: write `<exe>.new`, `chmod 755`, atomically `rename` over `<exe>`.
//! - Windows: a running `.exe` cannot be overwritten but *can* be renamed, so
//!   move it to `<exe>.old`, write the new file, and clean `.old` on next start.

use std::env;
use std::fs;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Upstream repository (`owner/name`), overridable for forks/mirrors.
pub fn repo() -> String {
    env::var("SYSVIEW_UPDATE_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "OCEANOFANYTHING/sysview".to_string())
}

/// The version this binary was built as.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Release asset name for the current platform, or `None` if unsupported.
pub fn target_asset() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("sysview-linux-x86_64-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("sysview-linux-aarch64-musl")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("sysview-windows-x86_64.exe")
    } else if cfg!(target_os = "macos") {
        Some("sysview-mac-universal")
    } else {
        None
    }
}

/// What `sysview update` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Check-only mode: a newer release exists.
    Available { current: String, latest: String },
    /// Check-only mode: already newest.
    UpToDate { version: String },
    /// A download was written into place.
    Updated { from: String, to: String },
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Resolved release metadata for this platform.
#[derive(Debug, Clone)]
pub struct Latest {
    pub version: String,
    pub asset_url: String,
    pub sums_url: Option<String>,
}

impl Latest {
    pub fn newer_than(&self, current: &str) -> bool {
        compare_versions(&self.version, current) > 0
    }
}

/// Fetches the newest release metadata for [`repo`].
pub fn latest() -> Result<Latest, String> {
    let asset_name =
        target_asset().ok_or_else(|| "no prebuilt binary for this platform".to_string())?;
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    let body = http_get(&url, 30)?;
    let rel: GhRelease =
        serde_json::from_slice(&body).map_err(|e| format!("release metadata was not JSON: {e}"))?;
    let asset = rel
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| format!("release {} has no asset {asset_name}", rel.tag_name))?;
    let sums_url = rel
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .map(|a| a.browser_download_url.clone());
    Ok(Latest {
        version: rel.tag_name.trim_start_matches('v').to_string(),
        asset_url: asset.browser_download_url.clone(),
        sums_url,
    })
}

/// Checks whether a newer release exists without downloading a binary.
pub fn check() -> Result<Outcome, String> {
    let cur = current_version().to_string();
    let rel = latest()?;
    if rel.newer_than(&cur) {
        Ok(Outcome::Available {
            current: cur,
            latest: rel.version,
        })
    } else {
        Ok(Outcome::UpToDate { version: cur })
    }
}

/// Downloads and installs the newest release. With `force`, reinstalls even
/// when the running version is already current.
pub fn perform(force: bool) -> Result<Outcome, String> {
    let cur = current_version().to_string();
    let rel = latest()?;
    if !force && !rel.newer_than(&cur) {
        return Ok(Outcome::UpToDate { version: cur });
    }
    let bytes = http_get(&rel.asset_url, 120)?;
    verify_checksum(&rel, &bytes)?;
    if bytes.len() < 1024 {
        return Err("downloaded file looks too small to be the binary".to_string());
    }
    replace_self(&bytes)?;
    Ok(Outcome::Updated {
        from: cur,
        to: rel.version,
    })
}

/// Removes a leftover `<exe>.old` from a previous Windows update, if present.
/// Safe to call on every start; a no-op on Unix and when nothing is stale.
pub fn cleanup_old_binary() {
    if let Ok(exe) = env::current_exe() {
        let old = exe.with_extension("old");
        let _ = fs::remove_file(old);
    }
}

fn http_get(url: &str, timeout_secs: u64) -> Result<Vec<u8>, String> {
    let mut req = minreq::get(url)
        .with_timeout(timeout_secs)
        .with_header("User-Agent", concat!("sysview/", env!("CARGO_PKG_VERSION")))
        .with_header("Accept", "application/vnd.github+json");
    if let Ok(tok) = env::var("GITHUB_TOKEN")
        && !tok.trim().is_empty()
    {
        req = req.with_header("Authorization", format!("Bearer {tok}"));
    }
    let resp = req.send().map_err(|e| format!("network error: {e}"))?;
    if resp.status_code != 200 {
        return Err(format!("HTTP {} from {url}", resp.status_code));
    }
    Ok(resp.into_bytes())
}

fn verify_checksum(rel: &Latest, bytes: &[u8]) -> Result<(), String> {
    let Some(url) = &rel.sums_url else {
        return Ok(()); // no manifest published: proceed
    };
    let sums = match http_get(url, 30) {
        Ok(s) => s,
        Err(_) => return Ok(()), // manifest fetch failed: do not block the update
    };
    let text = String::from_utf8_lossy(&sums);
    let want = checksum_for(&text, &rel.asset_url);
    let Some(want) = want else {
        return Ok(()); // no line for our asset
    };
    let got = sha256_hex(bytes);
    if got.eq_ignore_ascii_case(&want) {
        Ok(())
    } else {
        Err("checksum mismatch — refusing to install a corrupted download".to_string())
    }
}

/// Finds the hex digest for the asset basename in a `SHA256SUMS` file.
fn checksum_for(sums: &str, asset_url: &str) -> Option<String> {
    let base = asset_url.rsplit('/').next()?;
    for line in sums.lines() {
        let mut it = line.split_whitespace();
        let (Some(hash), Some(name)) = (it.next(), it.next()) else {
            continue;
        };
        let name = name.trim_start_matches('*');
        if name == base && hash.len() == 64 {
            return Some(hash.to_string());
        }
    }
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Atomically installs `bytes` as the current executable.
fn replace_self(bytes: &[u8]) -> Result<(), String> {
    let exe = env::current_exe().map_err(|e| format!("cannot locate the running binary: {e}"))?;
    let tmp = exe.with_extension("new");
    fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755));
        fs::rename(&tmp, &exe).map_err(|e| format!("cannot replace {}: {e}", exe.display()))?;
    }
    #[cfg(windows)]
    {
        let old = exe.with_extension("old");
        let _ = fs::remove_file(&old);
        fs::rename(&exe, &old).map_err(|e| format!("cannot stage {}: {e}", exe.display()))?;
        if let Err(e) = fs::rename(&tmp, &exe) {
            let _ = fs::rename(&old, &exe); // roll back
            return Err(format!("cannot install {}: {e}", exe.display()));
        }
    }
    Ok(())
}

/// Numeric version comparison (`1.10.0` > `1.9.9`). Non-numeric segments are
/// compared as zero, so odd tags never panic.
fn compare_versions(a: &str, b: &str) -> i64 {
    let pa = parse_version(a);
    let pb = parse_version(b);
    for i in 0..pa.len().max(pb.len()) {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y {
            return (x as i64) - (y as i64);
        }
    }
    0
}

fn parse_version(v: &str) -> Vec<u64> {
    v.trim_start_matches('v')
        .split(['.', '-', '+'])
        .map(|seg| {
            let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().unwrap_or(0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_is_numeric() {
        assert!(compare_versions("1.10.0", "1.9.9") > 0);
        assert!(compare_versions("0.2.0", "0.1.6") > 0);
        assert_eq!(compare_versions("1.2.3", "1.2.3"), 0);
        assert!(compare_versions("1.2.3", "1.2.4") < 0);
        assert_eq!(compare_versions("v1.2", "1.2.0"), 0);
    }

    #[test]
    fn checksum_lookup_matches_basename() {
        let win = "a".repeat(64);
        let lin = "d".repeat(64);
        let sums = format!("{win}  sysview-windows-x86_64.exe\n{lin}  sysview-linux-x86_64-musl\n");
        let h = checksum_for(
            &sums,
            "https://example.com/download/sysview-linux-x86_64-musl",
        );
        assert_eq!(h.as_deref(), Some(lin.as_str()));
        // Wrong length digests are ignored.
        let short = "abc  x\n";
        assert!(checksum_for(short, "https://e/x").is_none());
    }

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256("abc")
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn target_asset_exists_for_host() {
        // On the platforms we build for, a name is available.
        if cfg!(any(
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "windows", target_arch = "x86_64"),
            target_os = "macos"
        )) {
            assert!(target_asset().is_some());
        }
    }
}
