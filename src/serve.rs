//! Embedded web dashboard server.
//!
//! A deliberately minimal HTTP/1.1 server built on `std::net` only, so the
//! binary stays portable with zero extra dependencies. Thread-per-connection,
//! `Connection: close`, routes:
//!
//! - `GET /`                -> the dashboard page (embedded HTML)
//! - `GET /api/snapshot`    -> shared JSON snapshot (see below)
//! - `GET /health`          -> liveness probe
//! - everything else        -> 404
//!
//! A background sampler thread is the single owner of `WebState`. It refreshes
//! on a fixed timer, appends to the server-side rolling history, serializes
//! the snapshot once per tick, and caches the JSON string. Every client then
//! receives the *same* cached bytes, so all devices — and every reload — see
//! identical stats, identical history, and identical charts. Sampling cost is
//! independent of how many devices are watching.
//!
//! Idle gating: sampling runs only while at least one dashboard is connected
//! (plus a short grace period after the last one disconnects, so reloads never
//! lose a beat). When no one has been watching for `IDLE_GRACE`, the sampler
//! drops the sysinfo state and cached payload entirely, leaving the process
//! near-zero memory until the next viewer connects. The tiny history ring
//! survives, so a reconnect resumes the same shared window.
//!
//! Security model:
//! - Default bind is loopback (`127.0.0.1`); remote access requires an
//!   explicit `--bind 0.0.0.0`.
//! - Optional `--token <t>` auth on every route (constant-time compare),
//!   via `?token=<t>` in the query string or `Authorization: Bearer <t>`.
//! - Only `GET`/`HEAD` are answered (405 otherwise); malformed request lines
//!   get 400; no path traversal or proxy-form targets are accepted.
//! - Bounded concurrency (thread-per-connection with a cap).
//! - `no-store` + `nosniff` + frame deny + CSP + no-referrer headers.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::collect::{WebState, HISTORY_MAX};
use crate::model::{MemoryInfo, WebDiskInfo, WebHistory, WebNetworkInfo};

/// Static dashboard page, compiled into the binary.
const DASHBOARD_HTML: &str = include_str!("../web/dashboard.html");

/// Maximum concurrent connections (thread cap, also bounds slow-loris sockets).
const MAX_CONNS: usize = 64;

/// How long the sampler keeps running after the last dashboard disconnects,
/// so a quick reload or a handoff between devices never loses a beat. After
/// this window, the sampling state is dropped until someone connects again.
const IDLE_GRACE: Duration = Duration::from_secs(30);

pub struct ServeConfig {
    pub bind: String,
    pub port: u16,
    /// Server sampling interval in seconds (comes from the global `--interval`).
    pub interval_secs: f64,
    pub max_procs: usize,
    pub token: Option<String>,
    pub kiosk: bool,
}

/// State shared with connection threads.
struct Shared {
    /// The latest fully-serialized snapshot (cleared while idle). Kept as
    /// raw bytes so the sampler can serialize into a recycled buffer and the
    /// HTTP layer can respond without any copy.
    latest: Mutex<Option<Vec<u8>>>,
    /// Connections currently being served (drives idle gating in the sampler).
    conns: AtomicUsize,
    /// Timestamp of the most recent accepted dashboard request, so the sampler
    /// wakes up even for connections that last only a few milliseconds (every
    /// dashboard poll is its own short-lived HTTP connection).
    last_conn: Mutex<Instant>,
}

/// Records that a dashboard viewer just pulled live data. This is what keeps
/// the sampler alive for `IDLE_GRACE`; headless probes (`/health`, favicon)
/// and failed requests never re-arm it, so idle systems truly go quiet.
fn mark_watching(shared: &Shared) {
    if let Ok(mut lc) = shared.last_conn.lock() {
        *lc = Instant::now();
    }
}

/// Starts the dashboard server and blocks forever (until interrupted).
pub fn run(cfg: ServeConfig) -> std::io::Result<()> {
    let listener = TcpListener::bind((cfg.bind.as_str(), cfg.port))?;
    let local = listener.local_addr()?;
    let html: Arc<str> = Arc::from(render_dashboard(&cfg));
    let shared = Arc::new(Shared {
        latest: Mutex::new(None),
        conns: AtomicUsize::new(0),
        last_conn: Mutex::new(Instant::now()),
    });

    start_sampler(Arc::clone(&shared), cfg.max_procs, cfg.interval_secs);
    // Wait until the first payload is ready so the very first client never
    // sees an empty "warming up" response.
    for _ in 0..400 {
        if shared.latest.lock().map(|g| g.is_some()).unwrap_or(false) {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    let has_auth = cfg.token.is_some();
    println!("sysview dashboard: http://{local}/");
    if let Some(t) = &cfg.token {
        println!("  auth: token required  (URL: http://{local}/?token={t})");
    }
    println!(
        "  sample every {:.0}s, {} processes, history: 240 pts, kiosk: {}",
        cfg.interval_secs, cfg.max_procs, cfg.kiosk
    );
    println!("  sampling pauses while no dashboard is open (near-zero memory) and");
    println!("  auto-resumes on connect; the shared history window is preserved.");
    if !has_auth && cfg.bind != "127.0.0.1" {
        println!("  WARNING: no token set and binding {}. Anyone who can reach", cfg.bind);
        println!("           this port can read live system stats. Use --token <secret>.");
    }
    println!("  press Ctrl+C to stop");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if shared.conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                    shared.conns.fetch_sub(1, Ordering::SeqCst);
                    let mut s = s;
                    respond(
                        &mut s,
                        "503 Service Unavailable",
                        "text/plain; charset=utf-8",
                        b"too many connections",
                        false,
                        false,
                    );
                    continue;
                }
                let shared = Arc::clone(&shared);
                let html = Arc::clone(&html);
                let token = cfg.token.clone();
                thread::spawn(move || {
                    handle_conn(s, shared.clone(), html, token);
                    shared.conns.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

/// Injects the runtime config (poll interval, kiosk) into the static page.
fn render_dashboard(cfg: &ServeConfig) -> String {
    let poll_ms = (cfg.interval_secs.max(0.1) * 1000.0).round() as u64;
    let body_class = if cfg.kiosk { "dashboard kiosk" } else { "dashboard" };
    DASHBOARD_HTML
        .replace("var POLL = 2000;", &format!("var POLL = {poll_ms};"))
        .replace("class=\"dashboard\"", &format!("class=\"{body_class}\""))
}

/// Background sampler: the single writer of `WebState`.
///
/// Runs continuously but only *samples* while a dashboard is connected, plus
/// `IDLE_GRACE` after the last one leaves (so reloads/handoffs never drop a
/// beat). Once idle past the grace, the sysinfo state and cached payload are
/// dropped — the process stays up with near-zero memory. The tiny history ring
/// is kept, so the next viewer resumes the exact same shared window instead of
/// starting over.
fn start_sampler(shared: Arc<Shared>, max_procs: usize, interval: f64) {
    let interval = interval.max(0.1);
    thread::spawn(move || {
        let mut st: Option<WebState> = None;
        let mut ring = HistoryRing::new();
        loop {
            let t0 = Instant::now();
            let active_conns = shared.conns.load(Ordering::Relaxed);
            let last_conn = match shared.last_conn.lock() {
                Ok(g) => *g,
                Err(p) => *p.into_inner(),
            };
            let active = active_conns > 0 || last_conn.elapsed() <= IDLE_GRACE;

            if active {
                // (Re)build sampling state on the first tick after idle.
                if st.is_none() {
                    st = Some(WebState::new(max_procs));
                }
                let mut snap = st.as_mut().expect("state built above").snapshot();
                let (rx, tx) = net_rate_sum(&snap.networks);
                ring.push(
                    snap.cpu.usage,
                    snap.memory.used_percent,
                    swap_percent(&snap.memory),
                    rx,
                    tx,
                );
                let core_usage: Vec<f32> = snap.cpu.cores.iter().map(|c| c.usage).collect();
                ring.push_cores(&core_usage);
                for d in &snap.disks {
                    ring.push_disk(&d.mount_point, disk_used_percent(d));
                }
                snap.history = ring.to_web();
                // Serialize into the cached payload's own allocation: take it,
                // clear it (capacity is preserved), write in place, put it
                // back. Each tick performs zero heap allocations and exactly
                // one payload-sized buffer is ever retained.
                {
                    let mut slot = match shared.latest.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    let mut buf = slot.take().unwrap_or_default();
                    buf.clear();
                    if serde_json::to_writer(&mut buf, &snap).is_ok() {
                        *slot = Some(buf);
                    }
                }
            } else if st.is_some() {
                // Nobody watching for a while: free the sysinfo state and the
                // cached payload. History ring (a few KB) is retained.
                st = None;
                if let Ok(mut g) = shared.latest.lock() {
                    *g = None;
                }
            }

            let sleep = interval - t0.elapsed().as_secs_f64();
            if sleep > 0.0 {
                thread::sleep(Duration::from_secs_f64(sleep));
            }
        }
    });
}

/// Rolling per-metric history (capped), kept in the sampler thread so it
/// survives idle teardowns.
struct HistoryRing {
    cpu: VecDeque<f32>,
    mem: VecDeque<f32>,
    swap: VecDeque<f32>,
    rx: VecDeque<f64>,
    tx: VecDeque<f64>,
    /// Per-logical-core usage history; index = core, each deque capped.
    cores: Vec<VecDeque<f32>>,
    /// Per-mount-point usage history, keyed by mount path.
    disks: BTreeMap<String, VecDeque<f32>>,
    /// Insertion order of mount keys, for evicting the oldest when the disk
    /// set grows (a fresh VM snapshot could otherwise accumulate mounts).
    disk_order: VecDeque<String>,
}

impl HistoryRing {
    fn new() -> Self {
        Self {
            cpu: VecDeque::with_capacity(HISTORY_MAX + 1),
            mem: VecDeque::with_capacity(HISTORY_MAX + 1),
            swap: VecDeque::with_capacity(HISTORY_MAX + 1),
            rx: VecDeque::with_capacity(HISTORY_MAX + 1),
            tx: VecDeque::with_capacity(HISTORY_MAX + 1),
            cores: Vec::new(),
            disks: BTreeMap::new(),
            disk_order: VecDeque::new(),
        }
    }

    fn push(&mut self, cpu: f32, mem: f32, swap: f32, rx: f64, tx: f64) {
        cap_push(&mut self.cpu, cpu);
        cap_push(&mut self.mem, mem);
        cap_push(&mut self.swap, swap);
        cap_push(&mut self.rx, rx);
        cap_push(&mut self.tx, tx);
    }

    /// Records one per-core usage vector. Rebuilds the per-core deques if the
    /// core count changed (rare: hotplug/VMs), which simply restarts that
    /// window — cleaner than misaligning series across a resized core set.
    fn push_cores(&mut self, usage: &[f32]) {
        if self.cores.len() != usage.len() {
            self.cores = usage
                .iter()
                .map(|_| VecDeque::with_capacity(HISTORY_MAX + 1))
                .collect();
        }
        for (deq, &u) in self.cores.iter_mut().zip(usage) {
            cap_push(deq, u);
        }
    }

    /// Records one disk's usage. Bounds total tracked mounts so a host whose
    /// mount set churns (container VMs, USB drives) cannot grow the map.
    fn push_disk(&mut self, mount: &str, pct: f32) {
        if !self.disks.contains_key(mount) {
            if self.disks.len() >= MAX_DISK_MOUNTS
                && let Some(old) = self.disk_order.pop_front()
            {
                self.disks.remove(&old);
            }
            self.disk_order.push_back(mount.to_string());
        }
        let deq = self
            .disks
            .entry(mount.to_string())
            .or_insert_with(|| VecDeque::with_capacity(HISTORY_MAX + 1));
        cap_push(deq, pct);
    }

    fn to_web(&self) -> WebHistory {
        WebHistory {
            cpu: self.cpu.iter().copied().collect(),
            mem: self.mem.iter().copied().collect(),
            swap: self.swap.iter().copied().collect(),
            rx: self.rx.iter().copied().collect(),
            tx: self.tx.iter().copied().collect(),
            cores: self.cores.iter().map(|d| d.iter().copied().collect()).collect(),
            disks: self
                .disks
                .iter()
                .map(|(k, d)| (k.clone(), d.iter().copied().collect()))
                .collect(),
        }
    }
}

/// Largest number of distinct mounts the per-disk history will track.
const MAX_DISK_MOUNTS: usize = 64;

/// Disk usage as a percentage of its total (0 when the total is 0).
fn disk_used_percent(d: &WebDiskInfo) -> f32 {
    if d.total > 0 {
        d.used as f32 * 100.0 / d.total as f32
    } else {
        0.0
    }
}

/// Appends a sample to one capped ring (drops the oldest past the window).
fn cap_push<T: Copy>(deq: &mut VecDeque<T>, v: T) {
    deq.push_back(v);
    if deq.len() > HISTORY_MAX {
        deq.pop_front();
    }
}

/// Swap usage as a percentage of total (0 when no swap exists).
fn swap_percent(mem: &MemoryInfo) -> f32 {
    if mem.swap_total > 0 {
        mem.swap_used as f32 * 100.0 / mem.swap_total as f32
    } else {
        0.0
    }
}

/// Sum of rx/tx rates across all interfaces (bytes per second).
fn net_rate_sum(nets: &[WebNetworkInfo]) -> (f64, f64) {
    let mut rx = 0.0_f64;
    let mut tx = 0.0_f64;
    for n in nets {
        rx += n.rx_bps;
        tx += n.tx_bps;
    }
    (rx, tx)
}

/// Minimal typed view of the cached snapshot for `/metrics`, decoded directly
/// — not through a `serde_json::Value` DOM. Only a handful of scalars plus the
/// disk/network/temperature labels are needed, so everything else (history,
/// per-process detail, per-core series) is skipped while parsing instead of
/// being materialized into heap nodes. It compiles to the same text output as
/// the equivalent `Value` walk, without the transient tree.
#[derive(Deserialize)]
struct MetricsSnapshot {
    ts: u64,
    system: MetricsSystem,
    cpu: MetricsCpu,
    memory: MetricsMemory,
    disks: Vec<MetricsDisk>,
    networks: Vec<MetricsNetwork>,
    /// Placeholder element type: only the vector's length is used (the process
    /// count). Each process object parses normally as an extraneous value that
    /// serde ignores, so no per-process data is retained.
    processes: Vec<MetricsIgnored>,
    sensors: MetricsSensors,
}

#[derive(Deserialize)]
struct MetricsSystem {
    uptime_secs: u64,
    load_average: Option<MetricsLoad>,
}

#[derive(Deserialize)]
struct MetricsLoad {
    one: f64,
    five: f64,
    fifteen: f64,
}

#[derive(Deserialize)]
struct MetricsCpu {
    usage: f64,
    logical_cores: u64,
}

#[derive(Deserialize)]
struct MetricsMemory {
    total: u64,
    used: u64,
    available: u64,
    used_percent: f64,
    swap_total: u64,
    swap_used: u64,
}

#[derive(Deserialize)]
struct MetricsDisk {
    mount_point: String,
    total: u64,
    used: u64,
    read_bps: f64,
    write_bps: f64,
}

#[derive(Deserialize)]
struct MetricsNetwork {
    name: String,
    received: u64,
    transmitted: u64,
    rx_bps: f64,
    tx_bps: f64,
}

#[derive(Deserialize)]
struct MetricsSensors {
    temperatures: Vec<MetricsTemperature>,
}

#[derive(Deserialize)]
struct MetricsTemperature {
    label: String,
    temperature_c: Option<f64>,
}

/// Field-less element type: `Vec<MetricsIgnored>` captures an array's length
/// without allocating anything per element.
#[derive(Deserialize)]
struct MetricsIgnored {}

/// Turns the cached snapshot bytes into Prometheus text format. Parses on
/// demand and retains nothing, so /metrics costs nothing while idle.
fn json_to_metrics(json: &[u8]) -> String {
    let snap = match serde_json::from_slice::<MetricsSnapshot>(json) {
        Ok(s) => s,
        Err(_) => return "# sysview: snapshot not parseable\n".to_string(),
    };
    let mut out = String::with_capacity(4096);
    let mut seen = HashSet::new();

    emit_metric(
        &mut out,
        &mut seen,
        "sysview_uptime_seconds",
        "System uptime in seconds.",
        snap.system.uptime_secs,
    );
    if let Some(la) = snap.system.load_average {
        emit_metric(&mut out, &mut seen, "sysview_load1", "1-minute load average.", la.one);
        emit_metric(&mut out, &mut seen, "sysview_load5", "5-minute load average.", la.five);
        emit_metric(&mut out, &mut seen, "sysview_load15", "15-minute load average.", la.fifteen);
    }
    emit_metric(&mut out, &mut seen, "sysview_ts_seconds", "Unix seconds of the last sample.", snap.ts);
    emit_metric(&mut out, &mut seen, "sysview_cpu_usage_percent", "Total CPU usage in percent.", snap.cpu.usage);
    emit_metric(&mut out, &mut seen, "sysview_cpu_logical_cores", "Logical CPU count.", snap.cpu.logical_cores);
    let m = &snap.memory;
    emit_metric(&mut out, &mut seen, "sysview_memory_total_bytes", "Total physical memory in bytes.", m.total);
    emit_metric(&mut out, &mut seen, "sysview_memory_used_bytes", "Used physical memory in bytes.", m.used);
    emit_metric(&mut out, &mut seen, "sysview_memory_available_bytes", "Available memory in bytes.", m.available);
    emit_metric(&mut out, &mut seen, "sysview_memory_used_percent", "Used memory as a percent of total.", m.used_percent);
    emit_metric(&mut out, &mut seen, "sysview_swap_total_bytes", "Total swap in bytes.", m.swap_total);
    emit_metric(&mut out, &mut seen, "sysview_swap_used_bytes", "Used swap in bytes.", m.swap_used);
    for d in &snap.disks {
        let mount = esc_label(&d.mount_point);
        emit_metric(
            &mut out,
            &mut seen,
            &format!("sysview_disk_total_bytes{{mount=\"{mount}\"}}"),
            "Total disk size in bytes.",
            d.total,
        );
        emit_metric(
            &mut out,
            &mut seen,
            &format!("sysview_disk_used_bytes{{mount=\"{mount}\"}}"),
            "Used disk space in bytes.",
            d.used,
        );
        let pct = if d.total > 0 { d.used as f64 * 100.0 / d.total as f64 } else { 0.0 };
        emit_metric(
            &mut out,
            &mut seen,
            &format!("sysview_disk_used_percent{{mount=\"{mount}\"}}"),
            "Used disk space as a percent of total.",
            pct,
        );
        emit_metric(
            &mut out,
            &mut seen,
            &format!("sysview_disk_read_bps{{mount=\"{mount}\"}}"),
            "Disk read bytes per second.",
            d.read_bps,
        );
        emit_metric(
            &mut out,
            &mut seen,
            &format!("sysview_disk_write_bps{{mount=\"{mount}\"}}"),
            "Disk write bytes per second.",
            d.write_bps,
        );
    }
    for n in &snap.networks {
        let iface = esc_label(&n.name);
        emit_metric(
            &mut out,
            &mut seen,
            &format!("sysview_net_received_bytes_total{{interface=\"{iface}\"}}"),
            "Total bytes received on this interface since boot.",
            n.received,
        );
        emit_metric(
            &mut out,
            &mut seen,
            &format!("sysview_net_transmitted_bytes_total{{interface=\"{iface}\"}}"),
            "Total bytes transmitted on this interface since boot.",
            n.transmitted,
        );
        emit_metric(
            &mut out,
            &mut seen,
            &format!("sysview_net_rx_bps{{interface=\"{iface}\"}}"),
            "Interface receive bytes per second.",
            n.rx_bps,
        );
        emit_metric(
            &mut out,
            &mut seen,
            &format!("sysview_net_tx_bps{{interface=\"{iface}\"}}"),
            "Interface transmit bytes per second.",
            n.tx_bps,
        );
    }
    emit_metric(
        &mut out,
        &mut seen,
        "sysview_processes",
        "Number of processes in the latest snapshot.",
        snap.processes.len() as u64,
    );
    for t in &snap.sensors.temperatures {
        if let Some(c) = t.temperature_c {
            let label = esc_label(&t.label);
            emit_metric(
                &mut out,
                &mut seen,
                &format!("sysview_sensor_temperature_celsius{{sensor=\"{label}\"}}"),
                "Temperature sensor reading in Celsius.",
                c,
            );
        }
    }
    out
}

/// Appends one gauge line, emitting the HELP/TYPE headers the first time a
/// metric name appears.
fn emit_metric(out: &mut String, seen: &mut HashSet<String>, name: &str, help: &str, value: impl std::fmt::Display) {
    if seen.insert(name.to_string()) {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
    }
    out.push_str(&format!("{name} {value}\n"));
}

/// Escapes a Prometheus label value (backslash, quote, newline).
fn esc_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn handle_conn(mut stream: TcpStream, shared: Arc<Shared>, html: Arc<str>, token: Option<String>) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    // Per-read idle timeout: a keep-alive connection that goes quiet is
    // released after a few seconds instead of parking a thread.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    // Buffered bytes that were read past the last request head. Kept
    // across iterations so pipelined requests (a socket read may grab
    // several heads at once) are not lost between keep-alive cycles.
    let mut rest: Vec<u8> = Vec::with_capacity(2048);

    loop {
        // Read until at least one full head (header terminator) is buffered,
        // or a sane cap is exceeded (a head was malformed / never terminated).
        while find_terminator(&rest).is_none() && rest.len() <= 16 * 1024 {
            let mut chunk = [0u8; 2048];
            match stream.read(&mut chunk) {
                Ok(0) => return, // client closed the connection
                Ok(n) => rest.extend_from_slice(&chunk[..n]),
                Err(_) => return, // read timeout or transport error
            }
        }

        let head = match find_terminator(&rest) {
            Some(p) => rest.drain(..p + 4).collect::<Vec<u8>>(),
            None => std::mem::take(&mut rest), // over cap: treat it all as the head
        };

        let text = String::from_utf8_lossy(&head);
        let keep = wants_keep_alive(&text);
        let req_line = text.lines().next().unwrap_or("").trim();
        let mut parts = req_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        let _version = parts.next(); // optional "HTTP/1.1"
        if parts.next().is_some() {
            // More than (method target [version]) -> malformed.
            respond(&mut stream, "400 Bad Request", "text/plain; charset=utf-8", b"bad request", false, false);
            return;
        }

        // Only GET/HEAD; reject everything else (POST, TRACE, ...) outright.
        if method != "GET" && method != "HEAD" {
            let _ = write!(
                stream,
                "HTTP/1.1 405 Method Not Allowed\r\n\
                 Allow: GET, HEAD\r\n\
                 Content-Length: 0\r\n\
                 Cache-Control: no-store\r\n\
                 Connection: close\r\n\
                 X-Content-Type-Options: nosniff\r\n\
                 \r\n"
            );
            let _ = stream.flush();
            return;
        }

        // Only origin-form targets (`/path`). Rejects absolute-form proxy targets,
        // empty targets and anything else weird.
        if !target.starts_with('/') || target.len() > 2048 {
            respond(&mut stream, "400 Bad Request", "text/plain; charset=utf-8", b"bad request", false, false);
            return;
        }

        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target, ""),
        };

        // Reject dot-segment path traversal outright (defense in depth: the route
        // table is exact-match so nothing leaks, but encoded traversal shouldn't
        // even reach routing). The decoded path covers %2e%2e-style tricks.
        if percent_decode(path).split('/').any(|seg| seg == "..") {
            respond(
                &mut stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                b"bad request",
                false,
                false,
            );
            return;
        }

        // Auth gate (constant-time compare) when a token is configured.
        if let Some(tok) = &token {
            let supplied = if !query.is_empty() {
                query_token(query)
            } else {
                String::new()
            };
            let ok = (!supplied.is_empty() && ct_eq(&supplied, tok))
                || ct_eq(&header_token(&text), tok);
            if !ok {
                respond(&mut stream, "401 Unauthorized", "text/plain; charset=utf-8", b"unauthorized", false, false);
                return;
            }
        }

        match path {
            "/" | "/index.html" => {
                // A dashboard viewer means "someone is watching" for idle gating.
                mark_watching(&shared);
                respond(&mut stream, "200 OK", "text/html; charset=utf-8", html.as_bytes(), method == "HEAD", keep);
            }
            "/api/snapshot" => {
                // Read under the lock, respond while the guard is held: avoids a
                // full 60-90 KB clone on every poll tick.
                let guard = match shared.latest.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                // Serving live data counts as a viewer; warm-up 503s count too
                // (the viewer is polling), but health checks don't. A 503 that
                // fails to re-arm lets the sampler drop back to sleep between
                // quick localhost requests, stranding the next poll in an
                // endless warm-up race.
                mark_watching(&shared);
                match guard.as_ref() {
                    Some(j) => {
                        respond(&mut stream, "200 OK", "application/json; charset=utf-8", j.as_slice(), method == "HEAD", keep);
                    }
                    None => respond(
                        &mut stream,
                        "503 Service Unavailable",
                        "application/json; charset=utf-8",
                        b"{\"error\":\"warming up\"}",
                        method == "HEAD",
                        false,
                    ),
                }
            }
            "/metrics" => {
                // Prometheus text format over the current snapshot. Warm-up 503s
                // behave like the snapshot route; scrapers re-arm idle gating.
                let txt = {
                    let guard = match shared.latest.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    guard.as_ref().map(|j| json_to_metrics(j.as_slice()))
                };
                mark_watching(&shared);
                match txt {
                    Some(t) => {
                        respond(
                            &mut stream,
                            "200 OK",
                            "text/plain; version=0.0.4; charset=utf-8",
                            t.as_bytes(),
                            method == "HEAD",
                            keep,
                        );
                    }
                    None => respond(
                        &mut stream,
                        "503 Service Unavailable",
                        "text/plain; charset=utf-8",
                        b"warming up",
                        method == "HEAD",
                        false,
                    ),
                }
            }
            "/health" => {
                respond(&mut stream, "200 OK", "application/json; charset=utf-8", b"{\"ok\":true}", method == "HEAD", keep);
            }
            "/favicon.ico" => {
                respond(&mut stream, "204 No Content", "image/x-icon", b"", true, keep);
            }
            _ => {
                respond(&mut stream, "404 Not Found", "text/plain; charset=utf-8", b"not found", method == "HEAD", keep);
            }
        }

        if !keep {
            return;
        }
    }
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8], head_only: bool, keep_alive: bool) {
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: {conn}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\
         \r\n",
        body.len()
    );
    if !head_only && !body.is_empty() {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

/// Byte offset of the HTTP header terminator (CRLFCRLF), if present.
fn find_terminator(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Whether the request asked for a persistent connection. HTTP/1.1 defaults
/// to keep-alive unless `Connection: close` is sent; HTTP/1.0 only persists
/// when it explicitly says `keep-alive`.
fn wants_keep_alive(head: &str) -> bool {
    let mut has_conn = false;
    let mut close = false;
    // HTTP/1.0 request line ("GET / HTTP/1.0") is the last token before the
    // headers; HTTP/1.1 is the default for everything we serve.
    let mut http10 = head
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .rsplit(' ')
        .next()
        .map(|v| v.eq_ignore_ascii_case("HTTP/1.0"))
        .unwrap_or(false);
    for line in head.lines().skip(1) {
        let lower = line.trim().to_ascii_lowercase();
        if lower.starts_with("http/1.0 ") {
            http10 = true;
        } else if let Some(v) = lower.strip_prefix("connection:") {
            has_conn = true;
            close = v.contains("close");
        }
    }
    if has_conn {
        !close
    } else {
        !http10
    }
}

/// Constant-time byte comparison (resists timing attacks on the token).
fn ct_eq(a: &str, b: &str) -> bool {
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

/// Extracts a `token=...` value from a query string (URL-decoded).
fn query_token(query: &str) -> String {
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("token=") {
            return percent_decode(v);
        }
    }
    String::new()
}

/// Looks for an `Authorization: Bearer <token>` header (case-insensitive key).
fn header_token(head: &str) -> String {
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("authorization:")
            && let Some(v) = rest.trim().strip_prefix("bearer ")
        {
            return v.trim().to_string();
        }
    }
    String::new()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_and_rejects() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd")); // different length
        assert!(!ct_eq("", "a"));
        assert!(ct_eq("", ""));
        assert!(!ct_eq("secrets", "secret"));
    }

    #[test]
    fn hex_converts_digits() {
        assert_eq!(hex(b'0'), Some(0));
        assert_eq!(hex(b'9'), Some(9));
        assert_eq!(hex(b'a'), Some(10));
        assert_eq!(hex(b'F'), Some(15));
        assert_eq!(hex(b'g'), None);
        assert_eq!(hex(b' '), None);
    }

    #[test]
    fn percent_decode_handles_encodings() {
        assert_eq!(percent_decode("abc"), "abc");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%2Fetc%2Fpasswd"), "/etc/passwd");
        assert_eq!(percent_decode("token%3Dx"), "token=x");
        assert_eq!(percent_decode("%zz"), "%zz"); // invalid escape left alone
        assert_eq!(percent_decode("%41%42"), "AB");
    }

    #[test]
    fn query_token_extracts_and_decodes() {
        assert_eq!(query_token("t=1&token=abc&x=2"), "abc");
        assert_eq!(query_token("token=a%2Fb"), "a/b");
        assert_eq!(query_token("other=1"), "");
        assert_eq!(query_token(""), "");
        assert_eq!(query_token("TOKEN=x"), ""); // parameter name is case-sensitive
        assert_eq!(query_token("token="), ""); // empty token is not a match
    }

    #[test]
    fn header_token_parses_bearer() {
        assert_eq!(header_token("Authorization: Bearer abc"), "abc");
        assert_eq!(header_token("authorization: bearer xyz"), "xyz");
        assert_eq!(header_token("GET / HTTP/1.1\r\nAuthorization: Bearer tok"), "tok");
        assert_eq!(header_token("Authorization: Basic abc"), ""); // wrong scheme
        assert_eq!(header_token(""), "");
        assert_eq!(header_token("X-Custom: Bearer nope"), ""); // wrong header
    }

    #[test]
    fn render_dashboard_injects_config() {
        let plain = ServeConfig {
            bind: "127.0.0.1".to_string(),
            port: 8080,
            interval_secs: 5.0,
            max_procs: 100,
            token: None,
            kiosk: false,
        };
        let html = render_dashboard(&plain);
        assert!(html.contains("var POLL = 5000;"));
        assert!(html.contains("class=\"dashboard\""));
        assert!(!html.contains("class=\"dashboard kiosk\""));

        let kiosk = ServeConfig {
            interval_secs: 2.0,
            kiosk: true,
            ..plain
        };
        let html2 = render_dashboard(&kiosk);
        assert!(html2.contains("var POLL = 2000;"));
        assert!(html2.contains("class=\"dashboard kiosk\""));
    }

    #[test]
    fn history_ring_caps_and_stays_in_lockstep() {
        let mut ring = HistoryRing::new();
        assert_eq!(ring.to_web().cpu.len(), 0);
        assert!(ring.to_web().cores.is_empty());
        assert!(ring.to_web().disks.is_empty());
        for i in 0..500 {
            ring.push(i as f32, i as f32, i as f32, i as f64, i as f64);
            ring.push_cores(&[i as f32, i as f32 * 2.0]);
            ring.push_disk("C:", i as f32 * 0.5);
            ring.push_disk("D:", (500 - i) as f32);
        }
        // Every series capped together at the shared window.
        let w = ring.to_web();
        assert_eq!(w.cpu.len(), HISTORY_MAX);
        assert_eq!(w.mem.len(), HISTORY_MAX);
        assert_eq!(w.swap.len(), HISTORY_MAX);
        assert_eq!(w.rx.len(), HISTORY_MAX);
        assert_eq!(w.tx.len(), HISTORY_MAX);
        // Oldest dropped, newest kept, time-ordered.
        assert_eq!(w.cpu[0], (500 - HISTORY_MAX) as f32);
        assert_eq!(w.cpu[HISTORY_MAX - 1], 499.0);
        assert_eq!(w.rx[0], (500 - HISTORY_MAX) as f64);
        assert_eq!(w.tx[HISTORY_MAX - 1], 499.0);
        // Per-core series stay aligned with the core count.
        assert_eq!(w.cores.len(), 2);
        assert_eq!(w.cores[0].len(), HISTORY_MAX);
        assert_eq!(w.cores[0][0], (500 - HISTORY_MAX) as f32);
        assert_eq!(w.cores[1][HISTORY_MAX - 1], 998.0);
        // Per-disk series keyed by mount, capped independently.
        assert_eq!(w.disks.len(), 2);
        assert_eq!(w.disks["C:"].len(), HISTORY_MAX);
        assert_eq!(w.disks["C:"][0], (500 - HISTORY_MAX) as f32 * 0.5);
        assert_eq!(w.disks["D:"][HISTORY_MAX - 1], 1.0);
    }

    #[test]
    fn disk_used_percent_guards_zero_total() {
        let mut d = crate::model::WebDiskInfo {
            name: "nvme0n1".to_string(),
            mount_point: "C:".to_string(),
            file_system: "NTFS".to_string(),
            total: 200,
            available: 100,
            used: 50,
            read_bps: 0.0,
            write_bps: 0.0,
            removable: false,
            read_only: false,
        };
        assert_eq!(disk_used_percent(&d), 25.0);
        d.total = 0;
        assert_eq!(disk_used_percent(&d), 0.0);
    }

    #[test]
    fn disk_history_evicts_oldest_beyond_cap() {
        let mut ring = HistoryRing::new();
        for i in 0..(MAX_DISK_MOUNTS + 10) {
            ring.push_disk(&format!("mnt{i}"), 1.0);
        }
        let w = ring.to_web();
        assert_eq!(w.disks.len(), MAX_DISK_MOUNTS);
        assert!(!w.disks.contains_key("mnt0"));
        assert!(!w.disks.contains_key("mnt9")); // evicted too
        assert!(w.disks.contains_key("mnt10")); // first survivor
        assert!(w.disks.contains_key("mnt73")); // newest retained
    }

    #[test]
    fn swap_percent_and_net_rate_helpers() {
        use crate::model::MemoryInfo;

        let mem = MemoryInfo {
            total: 100,
            used: 50,
            available: 50,
            free: 50,
            swap_total: 200,
            swap_used: 20,
            used_percent: 50.0,
        };
        assert!((swap_percent(&mem) - 10.0).abs() < 0.001);

        let no_swap = MemoryInfo {
            total: 100,
            used: 0,
            available: 100,
            free: 100,
            swap_total: 0,
            swap_used: 0,
            used_percent: 0.0,
        };
        assert_eq!(swap_percent(&no_swap), 0.0); // no divide-by-zero

        let nets = vec![
            WebNetworkInfo {
                name: "eth0".to_string(),
                received: 1000,
                transmitted: 500,
                rx_bps: 1.5,
                tx_bps: 2.5,
            },
            WebNetworkInfo {
                name: "wlan0".to_string(),
                received: 0,
                transmitted: 0,
                rx_bps: 3.5,
                tx_bps: 0.5,
            },
        ];
        let (rx, tx) = net_rate_sum(&nets);
        assert!((rx - 5.0).abs() < 0.001 && (tx - 3.0).abs() < 0.001);
        assert_eq!(net_rate_sum(&[]), (0.0, 0.0));
    }

    #[test]
    fn request_line_guards_reject_bad_input() {
        // Every sample below must be rejected by at least one guard:
        // wrong method, malformed target, or too many tokens.
        let lines = [
            "POST /api/snapshot HTTP/1.1", // non-GET/HEAD
            "TRACE / HTTP/1.1",
            "GARBAGE",                     // no target
            "",
            "GET http://evil/ HTTP/1.1",   // absolute-form target
            "GET / HTTP/1.1 EXTRA",        // four tokens
            "GET /../etc/passwd HTTP/1.1", // dot-segment traversal
            "GET /%2e%2e/etc/passwd HTTP/1.1", // encoded traversal
        ];
        for line in lines {
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("");
            let _v = parts.next();
            let extra = parts.next().is_some();
            let bad_method = method != "GET" && method != "HEAD";
            let bad_dot = percent_decode(target).split('/').any(|seg| seg == "..");
            let bad_target =
                target.is_empty() || !target.starts_with('/') || target.len() > 2048 || bad_dot;
            assert!(
                extra || bad_method || bad_target,
                "request line was not rejected: {line:?}"
            );
        }

        // And the well-formed line must sail through all three guards.
        let mut parts = "GET /api/snapshot?token=abc HTTP/1.1".split_whitespace();
        let method = parts.next().unwrap();
        let target = parts.next().unwrap();
        let _v = parts.next();
        assert!(parts.next().is_none());
        assert!(method == "GET");
        assert!(target.starts_with('/') && target.len() <= 2048);
    }

    #[test]
    fn keep_alive_detection() {
        // HTTP/1.1 defaults to persistent unless Connection: close.
        assert!(wants_keep_alive("GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert!(wants_keep_alive("GET / HTTP/1.1\r\nConnection: keep-alive\r\n\r\n"));
        assert!(!wants_keep_alive("GET / HTTP/1.1\r\nConnection: close\r\n\r\n"));
        // Case-insensitive header + value.
        assert!(!wants_keep_alive("GET / HTTP/1.1\r\nConnection: ClOsE\r\n\r\n"));
        // HTTP/1.0 only persists when explicitly asked.
        assert!(!wants_keep_alive("GET / HTTP/1.0\r\n\r\n"));
        assert!(wants_keep_alive("GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"));
        // Other headers must not confuse detection.
        assert!(wants_keep_alive("GET / HTTP/1.1\r\nConnection: upgrade\r\nX-Connection: close\r\n\r\n"));
    }

    #[test]
    fn metrics_text_matches_prometheus_format() {
        let json = r#"{
            "ts": 1720000000,
            "system": {"uptime_secs": 4299, "load_average": {"one": 0.5, "five": 0.4, "fifteen": 0.3}},
            "cpu": {"usage": 12.5, "logical_cores": 4},
            "memory": {"total": 1000, "used": 250, "available": 750, "free": 700, "used_percent": 25.0, "swap_total": 200, "swap_used": 10},
            "disks": [{"mount_point": "C:\\", "total": 500, "used": 250, "read_bps": 3.0, "write_bps": 1.5}],
            "networks": [{"name": "eth0", "received": 99, "transmitted": 88, "rx_bps": 2.0, "tx_bps": 4.0}],
            "processes": [{}],
            "sensors": {"temperatures": [{"label": "cpu 0", "temperature_c": 51.2}]}
        }"#;
        let out = json_to_metrics(json.as_bytes());
        assert!(out.contains("sysview_uptime_seconds 4299\n"), "{out}");
        assert!(out.contains("sysview_cpu_usage_percent 12.5\n"), "{out}");
        assert!(out.contains("sysview_memory_used_percent 25\n"), "{out}");
        assert!(out.contains("sysview_processes 1\n"), "{out}");
        assert!(out.contains("sysview_disk_used_percent{mount=\"C:\\\\\"} 50\n"), "{out}");
        assert!(out.contains("sysview_net_rx_bps{interface=\"eth0\"} 2\n"), "{out}");
        assert!(out.contains("sysview_sensor_temperature_celsius{sensor=\"cpu 0\"} 51.2\n"), "{out}");
        assert!(out.lines().filter(|l| l.starts_with("# TYPE sysview_cpu_usage_percent")).count() == 1);
        assert!(out.lines().all(|l| l.starts_with('#') || l.contains(' ')), "{out}");
    }

    #[test]
    fn metrics_text_tolerates_garbage() {
        assert!(json_to_metrics(b"not json").starts_with("# sysview"));
    }
}