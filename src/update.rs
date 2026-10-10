//! Self-update from GitHub Releases — no external tools (no `git`, no `curl`).
//!
//! A tiny bundled HTTPS client (rustls with the compact `ring` provider, plus a
//! minimal HTTP/1.1 reader) fetches the latest release metadata and the matching
//! prebuilt binary, verifies it against the release's `SHA256SUMS`, and swaps it
//! into place in the same directory as the running executable. The **token and
//! configuration are never touched**, so an existing dashboard URL keeps working
//! after an update.
//!
//! `ring` is used instead of rustls' default `aws-lc-rs` provider because the
//! latter statically links a large chunk of BoringSSL and added megabytes to the
//! binary (and therefore to the resident set of every running server).
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
use std::io::Read;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

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

/// How many redirects an update request will follow (GitHub sends release
/// asset downloads to a CDN host).
const MAX_REDIRECTS: usize = 5;

/// Minimal HTTP/1.1 GET over TLS. Only what GitHub needs: `Content-Length`
/// and chunked bodies, and a handful of redirects.
fn http_get(url: &str, timeout_secs: u64) -> Result<Vec<u8>, String> {
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let resp = fetch_once(&url, timeout)?;
        if (300..400).contains(&resp.status) {
            match resp.header("location") {
                Some(loc) => {
                    url = resolve(&url, loc);
                    continue;
                }
                None => return Err(format!("HTTP {} from {url}", resp.status)),
            }
        }
        if resp.status != 200 {
            return Err(format!("HTTP {} from {url}", resp.status));
        }
        return Ok(resp.body);
    }
    Err("too many redirects".to_string())
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A parsed absolute URL (enough for `http`/`https` GETs).
struct Url {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("not an absolute URL: {url}"))?;
    let https = match scheme {
        "https" => true,
        "http" => false,
        other => return Err(format!("unsupported URL scheme {other:?}")),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("bad port in {authority:?}"))?,
        ),
        None => (authority.to_string(), if https { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("missing host in {url}"));
    }
    Ok(Url {
        https,
        host,
        port,
        path: path.to_string(),
    })
}

/// Resolves a `Location` header against the URL it came from.
fn resolve(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let Ok(b) = parse_url(base) else {
        return location.to_string();
    };
    let scheme = if b.https { "https" } else { "http" };
    let host = if (b.https && b.port == 443) || (!b.https && b.port == 80) {
        b.host.clone()
    } else {
        format!("{}:{}", b.host, b.port)
    };
    if location.starts_with('/') {
        return format!("{scheme}://{host}{location}");
    }
    let dir = match b.path.rfind('/') {
        Some(i) => &b.path[..=i],
        None => "/",
    };
    format!("{scheme}://{host}{dir}{location}")
}

fn fetch_once(url: &str, timeout: Duration) -> Result<HttpResponse, String> {
    let u = parse_url(url)?;
    let host_header = if (u.https && u.port == 443) || (!u.https && u.port == 80) {
        u.host.clone()
    } else {
        format!("{}:{}", u.host, u.port)
    };
    let mut request = String::with_capacity(256);
    request.push_str("GET ");
    request.push_str(&u.path);
    request.push_str(" HTTP/1.1\r\nHost: ");
    request.push_str(&host_header);
    request.push_str("\r\nUser-Agent: sysview/");
    request.push_str(env!("CARGO_PKG_VERSION"));
    request.push_str("\r\nAccept: application/vnd.github+json\r\n");
    request.push_str("Accept-Encoding: identity\r\nConnection: close\r\n");
    if let Ok(tok) = env::var("GITHUB_TOKEN")
        && !tok.trim().is_empty()
    {
        request.push_str("Authorization: Bearer ");
        request.push_str(tok.trim());
        request.push_str("\r\n");
    }
    request.push_str("\r\n");

    let addrs = (u.host.as_str(), u.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {}: {e}", u.host))?;
    let mut last_err = String::from("no address");
    let mut chosen = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                chosen = Some(s);
                break;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    let mut sock = chosen.ok_or_else(|| format!("cannot connect to {}: {last_err}", u.host))?;
    let _ = sock.set_read_timeout(Some(timeout));
    let _ = sock.set_write_timeout(Some(timeout));
    let _ = sock.set_nodelay(true);

    let raw = if u.https {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .roots
            .extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name = rustls::pki_types::ServerName::try_from(u.host.clone())
            .map_err(|e| format!("invalid server name {}: {e}", u.host))?;
        let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
            .map_err(|e| format!("TLS setup failed: {e}"))?;
        let mut tls = rustls::StreamOwned::new(conn, sock);
        tls.write_all(request.as_bytes())
            .map_err(|e| format!("TLS write failed: {e}"))?;
        tls.flush().map_err(|e| format!("TLS flush failed: {e}"))?;
        read_all(&mut tls)?
    } else {
        sock.write_all(request.as_bytes())
            .map_err(|e| format!("write failed: {e}"))?;
        sock.flush().map_err(|e| format!("flush failed: {e}"))?;
        read_all(&mut sock)?
    };

    parse_response(&raw)
}

/// Reads until the response is complete (`Content-Length`/chunked) or the peer
/// closes. Reading to close is the fallback; stopping early avoids hanging when
/// a server keeps a keep-alive socket open despite `Connection: close`.
fn read_all<R: Read>(reader: &mut R) -> Result<Vec<u8>, String> {
    let mut buf = Vec::with_capacity(1 << 16);
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(buf),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if response_complete(&buf) {
                    return Ok(buf);
                }
            }
            Err(e) => {
                if response_complete(&buf) {
                    return Ok(buf);
                }
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("read failed: {e}"));
            }
        }
    }
}

fn response_complete(buf: &[u8]) -> bool {
    let Some((head_end, body_start)) = find(buf, b"\r\n\r\n")
        .map(|i| (i, i + 4))
        .or_else(|| find(buf, b"\n\n").map(|i| (i, i + 2)))
    else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let body = &buf[body_start..];
    if head.contains("chunked") && head.contains("transfer-encoding") {
        chunked_complete(body)
    } else if let Some(len) = header_value(&head, "content-length").and_then(|v| v.parse().ok()) {
        body.len() >= len
    } else {
        false // unknown length: wait for EOF
    }
}

fn chunked_complete(mut b: &[u8]) -> bool {
    loop {
        let Some(line_end) = find(b, b"\r\n").or_else(|| find(b, b"\n")) else {
            return false;
        };
        let line = String::from_utf8_lossy(&b[..line_end]);
        let Ok(size) = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
        else {
            return false;
        };
        let skip = if b[line_end..].starts_with(b"\r\n") {
            2
        } else {
            1
        };
        b = &b[line_end + skip..];
        if size == 0 {
            return true;
        }
        if b.len() < size {
            return false;
        }
        b = &b[size..];
        if b.starts_with(b"\r\n") {
            b = &b[2..];
        } else if b.starts_with(b"\n") {
            b = &b[1..];
        }
    }
}

/// Finds the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, String> {
    let (head_end, body_start) = find(raw, b"\r\n\r\n")
        .map(|i| (i, i + 4))
        .or_else(|| find(raw, b"\n\n").map(|i| (i, i + 2)))
        .ok_or_else(|| "malformed HTTP response (no header terminator)".to_string())?;
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let mut lines = head.split('\n');
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed HTTP status line: {status_line:?}"))?;
    let mut headers = Vec::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let body = &raw[body_start..];
    let chunked = headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let body = if chunked {
        dechunk(body)?
    } else if let Some(len) = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
    {
        body[..len.min(body.len())].to_vec()
    } else {
        body.to_vec()
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Case-insensitive header lookup in a raw (lower-cased) header block.
fn header_value<'a>(head_lower: &'a str, name: &str) -> Option<&'a str> {
    for line in head_lower.lines() {
        if let Some((n, v)) = line.split_once(':')
            && n.trim() == name
        {
            return Some(v.trim());
        }
    }
    None
}

fn dechunk(mut buf: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = find(buf, b"\r\n")
            .or_else(|| find(buf, b"\n"))
            .ok_or_else(|| "truncated chunked body".to_string())?;
        let line = String::from_utf8_lossy(&buf[..line_end]);
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| format!("bad chunk size {size_str:?}"))?;
        let skip = if buf[line_end..].starts_with(b"\r\n") {
            2
        } else {
            1
        };
        buf = &buf[line_end + skip..];
        if size == 0 {
            return Ok(out);
        }
        if buf.len() < size {
            return Err("truncated chunk".to_string());
        }
        out.extend_from_slice(&buf[..size]);
        buf = &buf[size..];
        if buf.starts_with(b"\r\n") {
            buf = &buf[2..];
        } else if buf.starts_with(b"\n") {
            buf = &buf[1..];
        }
    }
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

    #[test]
    fn parse_url_splits_scheme_host_port_path() {
        let u = parse_url("https://api.github.com/repos/x/y/releases/latest").unwrap();
        assert!(u.https);
        assert_eq!(u.host, "api.github.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/repos/x/y/releases/latest");

        let u = parse_url("http://example.com:8080/a").unwrap();
        assert!(!u.https);
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/a");

        // Bare host with no path defaults to "/".
        let u = parse_url("https://example.com").unwrap();
        assert_eq!(u.path, "/");

        assert!(parse_url("ftp://example.com/x").is_err());
        assert!(parse_url("not a url").is_err());
    }

    #[test]
    fn resolve_follows_absolute_and_relative_locations() {
        assert_eq!(
            resolve("https://github.com/a", "https://cdn.example/x"),
            "https://cdn.example/x"
        );
        assert_eq!(
            resolve("https://github.com/a/b", "/c"),
            "https://github.com/c"
        );
        assert_eq!(
            resolve("https://github.com/a/b/c", "d"),
            "https://github.com/a/b/d"
        );
        assert_eq!(
            resolve("http://example.com:8080/a/b", "c"),
            "http://example.com:8080/a/c"
        );
    }

    #[test]
    fn parse_response_honours_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Test: yes\r\n\r\nhello, world";
        let resp = parse_response(raw).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
        assert_eq!(resp.header("x-test"), Some("yes"));
    }

    #[test]
    fn parse_response_decodes_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let resp = parse_response(raw).unwrap();
        assert_eq!(resp.body, b"hello world");
    }

    #[test]
    fn response_complete_waits_for_the_whole_body() {
        assert!(!response_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel"
        ));
        assert!(response_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"
        ));
        assert!(!response_complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n"
        ));
        assert!(response_complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
        ));
        // Unknown length: only EOF can end it.
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\n\r\nanything"));
    }
}
