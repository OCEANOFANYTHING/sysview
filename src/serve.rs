//! Embedded web dashboard server.
//!
//! A deliberately minimal HTTP/1.1 server built on `std::net` only, so the
//! binary stays portable with zero extra dependencies. Thread-per-connection
//! with HTTP/1.1 keep-alive (quiet connections are released by a per-read
//! timeout instead of parking a thread), routes:
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
//!   explicit `--bind 0.0.0.0`. When bound beyond loopback the daemon prints
//!   the LAN URL (`http://<this-host-ip>:<port>/`) to open from any device.
//! - Optional `--token <t>` auth on every route (constant-time compare),
//!   via `?token=<t>` in the query string or `Authorization: Bearer <t>`.
//! - Only `GET`/`HEAD` are answered (405 otherwise); malformed request lines
//!   get 400; no path traversal or proxy-form targets are accepted.
//! - Bounded concurrency (thread-per-connection with a cap).
//! - Per-connection read/write timeouts (5 s / 15 s) release quiet or stuck
//!   clients; all timing knobs live in [`Timeouts`].
//! - `no-store` + `nosniff` + frame deny + CSP + no-referrer headers.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::collect::{HISTORY_MAX, WebState};
use crate::model::{MemoryInfo, WebDiskInfo, WebHistory, WebNetworkInfo};

/// Static dashboard page, compiled into the binary.
const DASHBOARD_HTML: &str = include_str!("../web/dashboard.html");

/// Maximum concurrent connections (thread cap, also bounds slow-loris sockets).
const MAX_CONNS: usize = 64;

/// Stack size for connection-handler threads. Handlers parse request heads
/// and write responses; the one heavier path — the /metrics route — parses
/// JSON at a bounded depth of ~6 levels and converts to a heap string, so
/// even it stays within a few KiB of stack. A small, capped stack keeps the
/// worst-case commit of all connection slots bounded (64 x 256 KiB = 16 MiB)
/// instead of the ~1-2 MiB per-thread reserve `thread::spawn` would default
/// to (2 MiB on Linux, 1 MiB on Windows).
const CONN_STACK_SIZE: usize = 256 * 1024;

/// How long the sampler keeps running after the last dashboard disconnects,
/// so a quick reload or a handoff between devices never loses a beat. After
/// this window, the sampling state is dropped until someone connects again.
const IDLE_GRACE: Duration = Duration::from_secs(30);

/// Per-read idle cutoff: a keep-alive connection that goes quiet is released
/// after this instead of parking its thread.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-write cutoff: a client that stops draining a response is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-connection timing knobs. Tests shrink these so idle gating, read
/// timeouts and warm-up rebuilds complete in milliseconds instead of seconds;
/// production always uses [`Timeouts::default`] (the constants above).
#[derive(Clone, Copy)]
struct Timeouts {
    /// How long the sampler keeps sampling after the last connection leaves.
    idle_grace: Duration,
    /// Per-read idle cutoff for a quiet keep-alive connection.
    read: Duration,
    /// Per-write cutoff for a stuck client.
    write: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            idle_grace: IDLE_GRACE,
            read: READ_TIMEOUT,
            write: WRITE_TIMEOUT,
        }
    }
}

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
    /// Test-only observability: true while the sampler is actively sampling,
    /// false once it has dropped the state for idle. In production the
    /// observable signal is the payload 503/200 transition.
    #[cfg(test)]
    sampling: AtomicBool,
    /// Test-only: how many excess connections were refused with a 503 while
    /// the server was at the connection cap. Asserted from the server side so
    /// connection-limit tests never depend on a client successfully reading
    /// the refusal body.
    #[cfg(test)]
    rejected: AtomicUsize,
}

/// Records that a dashboard viewer just pulled live data. This is what keeps
/// the sampler alive for `IDLE_GRACE`; headless probes (`/health`, favicon)
/// and failed requests never re-arm it, so idle systems truly go quiet.
fn mark_watching(shared: &Shared) {
    if let Ok(mut lc) = shared.last_conn.lock() {
        *lc = Instant::now();
    }
}

/// True when the bind address exposes the dashboard beyond loopback (i.e.
/// any device on the network can attempt a connection).
fn is_lan_bind(bind: &str) -> bool {
    !matches!(bind, "127.0.0.1" | "::1" | "localhost")
}

/// Best-effort guess of this host's primary LAN IPv4, via the classic
/// route-discovery trick: a UDP `connect` only selects the egress interface
/// (no packet is ever sent), so `local_addr` reveals the address the kernel
/// would use to reach the peer. `None` when there is no usable route — the
/// daemon still runs, the LAN hint just falls back to a generic line.
fn detected_lan_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    sock.connect(("8.8.8.8", 80)).ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(v4) if !v4.ip().is_loopback() => Some(*v4.ip()),
        _ => None,
    }
}

/// Starts the dashboard server and blocks forever (until interrupted).
pub fn run(cfg: ServeConfig) -> std::io::Result<()> {
    let listener = TcpListener::bind((cfg.bind.as_str(), cfg.port))?;
    let local = listener.local_addr()?;
    let server = Server::start(&cfg)?;

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
    if is_lan_bind(&cfg.bind) {
        let suffix = cfg
            .token
            .as_deref()
            .map(|t| format!("?token={t}"))
            .unwrap_or_default();
        match detected_lan_ipv4() {
            Some(ip) => {
                println!("  reachable from any device on the network:");
                println!("    http://{ip}:{}/{}", cfg.port, suffix);
            }
            None => {
                println!("  bound on all interfaces — open it from any device at");
                println!("    http://<this-host-ip>:{}/{}", cfg.port, suffix);
            }
        }
    }
    if !has_auth && cfg.bind != "127.0.0.1" {
        println!(
            "  WARNING: no token set and binding {}. Anyone who can reach",
            cfg.bind
        );
        println!("           this port can read live system stats. Use --token <secret>.");
    }
    println!("  press Ctrl+C to stop");

    server.accept_loop(listener);
    Ok(())
}

/// A running dashboard server: shared state, pre-rendered page, auth token and
/// timing knobs. Owns no threads itself: `run` drives [`Server::accept_loop`]
/// on the main thread; tests spawn it in a helper thread and signal `stop`.
struct Server {
    shared: Arc<Shared>,
    html: Arc<str>,
    token: Option<String>,
    timeouts: Timeouts,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// Spawns the sampler thread and waits for the first payload so the very
    /// first client never sees an empty "warming up" response. The caller
    /// passes the already-bound listener to [`Server::accept_loop`].
    fn start(cfg: &ServeConfig) -> std::io::Result<Self> {
        Self::start_with_timeouts(cfg, Timeouts::default())
    }

    fn start_with_timeouts(cfg: &ServeConfig, timeouts: Timeouts) -> std::io::Result<Self> {
        let html: Arc<str> = Arc::from(render_dashboard(cfg));
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            conns: AtomicUsize::new(0),
            last_conn: Mutex::new(Instant::now()),
            #[cfg(test)]
            sampling: AtomicBool::new(false),
            #[cfg(test)]
            rejected: AtomicUsize::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        start_sampler(
            Arc::clone(&shared),
            cfg.max_procs,
            cfg.interval_secs,
            timeouts.idle_grace,
            Arc::clone(&stop),
        );
        // Wait until the first payload is ready so the very first client never
        // sees an empty "warming up" response.
        for _ in 0..400 {
            if shared.latest.lock().map(|g| g.is_some()).unwrap_or(false) {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        Ok(Self {
            shared,
            html,
            token: cfg.token.clone(),
            timeouts,
            stop,
        })
    }

    /// The accept loop. The listener is polled non-blocking (5 ms) so the loop
    /// can observe `stop` and shut down cleanly; accepted streams are reset to
    /// blocking mode (they inherit the listener's non-blocking flag).
    fn accept_loop(&self, listener: TcpListener) {
        if listener.set_nonblocking(true).is_err() {
            return;
        }
        while !self.stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => {
                    let mut s = s;
                    let _ = s.set_nonblocking(false);
                    if self.shared.conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                        self.shared.conns.fetch_sub(1, Ordering::SeqCst);
                        #[cfg(test)]
                        self.shared.rejected.fetch_add(1, Ordering::Relaxed);
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
                    let shared = Arc::clone(&self.shared);
                    let html = Arc::clone(&self.html);
                    let token = self.token.clone();
                    let timeouts = self.timeouts;
                    match thread::Builder::new().stack_size(CONN_STACK_SIZE).spawn({
                        let shared = Arc::clone(&shared);
                        move || {
                            handle_conn(s, shared.clone(), html, token, timeouts);
                            shared.conns.fetch_sub(1, Ordering::SeqCst);
                        }
                    }) {
                        Ok(_) => {}
                        Err(_) => {
                            // Thread creation failed: close the stream (the
                            // closure drops with it) and free the slot so a
                            // stuck connection can't wedge the cap forever.
                            shared.conns.fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                }
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// Test accessors: whether the sampler currently holds state, has a live
    /// payload, and how many connection slots are occupied.
    #[cfg(test)]
    fn is_sampling(&self) -> bool {
        self.shared.sampling.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn has_payload(&self) -> bool {
        self.shared
            .latest
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    #[cfg(test)]
    fn conns(&self) -> usize {
        self.shared.conns.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn rejected(&self) -> usize {
        self.shared.rejected.load(Ordering::Relaxed)
    }
}

/// Injects the runtime config (poll interval, kiosk) into the static page.
fn render_dashboard(cfg: &ServeConfig) -> String {
    let poll_ms = (cfg.interval_secs.max(0.1) * 1000.0).round() as u64;
    let body_class = if cfg.kiosk {
        "dashboard kiosk"
    } else {
        "dashboard"
    };
    DASHBOARD_HTML
        .replace("var POLL = 2000;", &format!("var POLL = {poll_ms};"))
        .replace("class=\"dashboard\"", &format!("class=\"{body_class}\""))
}

/// Background sampler: the single writer of `WebState`.
///
/// Runs continuously but only *samples* while a dashboard is connected, plus
/// `idle_grace` after the last one leaves (so reloads/handoffs never drop a
/// beat). Once idle past the grace, the sysinfo state and cached payload are
/// dropped — the process stays up with near-zero memory. The tiny history ring
/// is kept, so the next viewer resumes the exact same shared window instead of
/// starting over. Returns the thread handle; signal `stop` to exit cleanly.
fn start_sampler(
    shared: Arc<Shared>,
    max_procs: usize,
    interval: f64,
    idle_grace: Duration,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    let interval = interval.max(0.1);
    thread::spawn(move || {
        let mut st: Option<WebState> = None;
        let mut ring = HistoryRing::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let t0 = Instant::now();
            let active_conns = shared.conns.load(Ordering::Relaxed);
            let last_conn = match shared.last_conn.lock() {
                Ok(g) => *g,
                Err(p) => *p.into_inner(),
            };
            let active = active_conns > 0 || last_conn.elapsed() <= idle_grace;

            if active {
                // (Re)build sampling state on the first tick after idle.
                if st.is_none() {
                    st = Some(WebState::new(max_procs));
                }
                #[cfg(test)]
                shared.sampling.store(true, Ordering::Relaxed);
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
                #[cfg(test)]
                shared.sampling.store(false, Ordering::Relaxed);
                if let Ok(mut g) = shared.latest.lock() {
                    *g = None;
                }
            }

            let sleep = interval - t0.elapsed().as_secs_f64();
            if sleep > 0.0 {
                thread::sleep(Duration::from_secs_f64(sleep));
            }
        }
    })
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
            cores: self
                .cores
                .iter()
                .map(|d| d.iter().copied().collect())
                .collect(),
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
        emit_metric(
            &mut out,
            &mut seen,
            "sysview_load1",
            "1-minute load average.",
            la.one,
        );
        emit_metric(
            &mut out,
            &mut seen,
            "sysview_load5",
            "5-minute load average.",
            la.five,
        );
        emit_metric(
            &mut out,
            &mut seen,
            "sysview_load15",
            "15-minute load average.",
            la.fifteen,
        );
    }
    emit_metric(
        &mut out,
        &mut seen,
        "sysview_ts_seconds",
        "Unix seconds of the last sample.",
        snap.ts,
    );
    emit_metric(
        &mut out,
        &mut seen,
        "sysview_cpu_usage_percent",
        "Total CPU usage in percent.",
        snap.cpu.usage,
    );
    emit_metric(
        &mut out,
        &mut seen,
        "sysview_cpu_logical_cores",
        "Logical CPU count.",
        snap.cpu.logical_cores,
    );
    let m = &snap.memory;
    emit_metric(
        &mut out,
        &mut seen,
        "sysview_memory_total_bytes",
        "Total physical memory in bytes.",
        m.total,
    );
    emit_metric(
        &mut out,
        &mut seen,
        "sysview_memory_used_bytes",
        "Used physical memory in bytes.",
        m.used,
    );
    emit_metric(
        &mut out,
        &mut seen,
        "sysview_memory_available_bytes",
        "Available memory in bytes.",
        m.available,
    );
    emit_metric(
        &mut out,
        &mut seen,
        "sysview_memory_used_percent",
        "Used memory as a percent of total.",
        m.used_percent,
    );
    emit_metric(
        &mut out,
        &mut seen,
        "sysview_swap_total_bytes",
        "Total swap in bytes.",
        m.swap_total,
    );
    emit_metric(
        &mut out,
        &mut seen,
        "sysview_swap_used_bytes",
        "Used swap in bytes.",
        m.swap_used,
    );
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
        let pct = if d.total > 0 {
            d.used as f64 * 100.0 / d.total as f64
        } else {
            0.0
        };
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
fn emit_metric(
    out: &mut String,
    seen: &mut HashSet<String>,
    name: &str,
    help: &str,
    value: impl std::fmt::Display,
) {
    if seen.insert(name.to_string()) {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
    }
    out.push_str(&format!("{name} {value}\n"));
}

/// Escapes a Prometheus label value (backslash, quote, newline).
fn esc_label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn handle_conn(
    mut stream: TcpStream,
    shared: Arc<Shared>,
    html: Arc<str>,
    token: Option<String>,
    timeouts: Timeouts,
) {
    let _ = stream.set_write_timeout(Some(timeouts.write));
    // Per-read idle timeout: a keep-alive connection that goes quiet is
    // released after a few seconds instead of parking a thread.
    let _ = stream.set_read_timeout(Some(timeouts.read));
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
            let ok =
                (!supplied.is_empty() && ct_eq(&supplied, tok)) || ct_eq(&header_token(&text), tok);
            if !ok {
                respond(
                    &mut stream,
                    "401 Unauthorized",
                    "text/plain; charset=utf-8",
                    b"unauthorized",
                    false,
                    false,
                );
                return;
            }
        }

        match path {
            "/" | "/index.html" => {
                // A dashboard viewer means "someone is watching" for idle gating.
                mark_watching(&shared);
                respond(
                    &mut stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    html.as_bytes(),
                    method == "HEAD",
                    keep,
                );
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
                        respond(
                            &mut stream,
                            "200 OK",
                            "application/json; charset=utf-8",
                            j.as_slice(),
                            method == "HEAD",
                            keep,
                        );
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
                respond(
                    &mut stream,
                    "200 OK",
                    "application/json; charset=utf-8",
                    b"{\"ok\":true}",
                    method == "HEAD",
                    keep,
                );
            }
            "/favicon.ico" => {
                respond(
                    &mut stream,
                    "204 No Content",
                    "image/x-icon",
                    b"",
                    true,
                    keep,
                );
            }
            _ => {
                respond(
                    &mut stream,
                    "404 Not Found",
                    "text/plain; charset=utf-8",
                    b"not found",
                    method == "HEAD",
                    keep,
                );
            }
        }

        if !keep {
            return;
        }
    }
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    ctype: &str,
    body: &[u8],
    head_only: bool,
    keep_alive: bool,
) {
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
    if has_conn { !close } else { !http10 }
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
    use std::net::SocketAddr;

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
        assert_eq!(
            header_token("GET / HTTP/1.1\r\nAuthorization: Bearer tok"),
            "tok"
        );
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
    fn lan_bind_only_for_remote_addresses() {
        assert!(is_lan_bind("0.0.0.0"));
        assert!(is_lan_bind("192.168.1.2"));
        assert!(!is_lan_bind("127.0.0.1"));
        assert!(!is_lan_bind("::1"));
        assert!(!is_lan_bind("localhost"));
    }

    #[test]
    fn lan_bind_serves_authed_requests() {
        let (addr, server, stop, handle) = start_test_server_on("0.0.0.0", Some("sekret"));
        // The wildcard listener covers all interfaces; loopback reaches it.
        let via = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), addr.port());
        wait_for_payload(via, &server, Some("sekret"), "LAN-bound payload");
        // Right after a wake the first snapshot can 503 once while the payload
        // rebuilds; keep polling (each poll re-arms the sampler) for live data.
        wait_until("LAN snapshot 200", Duration::from_secs(10), || {
            http_get(via, "/api/snapshot?token=sekret").status == 200
        });
        let ok = http_get(via, "/api/snapshot?token=sekret");
        assert_eq!(ok.status, 200);
        let page = http_get(via, "/?token=sekret");
        assert_eq!(page.status, 200);
        assert!(String::from_utf8_lossy(&page.body).contains("dashboard"));
        let denied = http_get(via, "/api/snapshot");
        assert_eq!(denied.status, 401);
        let wrong = http_get_with_auth(via, "/api/snapshot", "Authorization: Bearer nope");
        assert_eq!(wrong.status, 401);
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
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
            "GARBAGE", // no target
            "",
            "GET http://evil/ HTTP/1.1",       // absolute-form target
            "GET / HTTP/1.1 EXTRA",            // four tokens
            "GET /../etc/passwd HTTP/1.1",     // dot-segment traversal
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
        assert!(wants_keep_alive(
            "GET / HTTP/1.1\r\nConnection: keep-alive\r\n\r\n"
        ));
        assert!(!wants_keep_alive(
            "GET / HTTP/1.1\r\nConnection: close\r\n\r\n"
        ));
        // Case-insensitive header + value.
        assert!(!wants_keep_alive(
            "GET / HTTP/1.1\r\nConnection: ClOsE\r\n\r\n"
        ));
        // HTTP/1.0 only persists when explicitly asked.
        assert!(!wants_keep_alive("GET / HTTP/1.0\r\n\r\n"));
        assert!(wants_keep_alive(
            "GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"
        ));
        // Other headers must not confuse detection.
        assert!(wants_keep_alive(
            "GET / HTTP/1.1\r\nConnection: upgrade\r\nX-Connection: close\r\n\r\n"
        ));
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
        assert!(
            out.contains("sysview_disk_used_percent{mount=\"C:\\\\\"} 50\n"),
            "{out}"
        );
        assert!(
            out.contains("sysview_net_rx_bps{interface=\"eth0\"} 2\n"),
            "{out}"
        );
        assert!(
            out.contains("sysview_sensor_temperature_celsius{sensor=\"cpu 0\"} 51.2\n"),
            "{out}"
        );
        assert!(
            out.lines()
                .filter(|l| l.starts_with("# TYPE sysview_cpu_usage_percent"))
                .count()
                == 1
        );
        assert!(
            out.lines().all(|l| l.starts_with('#') || l.contains(' ')),
            "{out}"
        );
    }

    #[test]
    fn metrics_text_tolerates_garbage() {
        assert!(json_to_metrics(b"not json").starts_with("# sysview"));
    }

    // --- integration helpers -------------------------------------------------

    /// Shrunk timeouts so idle gating and read timeouts settle in a few
    /// seconds instead of the production 30 s / 5 s windows. `idle_grace` is
    /// kept comfortably above a single `WebState::new` warmup pass (~0.5 s on
    /// a loaded host) so one build always fits inside the window, and the
    /// first-payload waits below re-arm the sampler if a straggling build
    /// drops one early — the mechanism under test is the idle transition
    /// itself, not the 30 s duration (production's grace is unaffected).
    fn test_timeouts() -> Timeouts {
        Timeouts {
            idle_grace: Duration::from_millis(1000),
            read: Duration::from_millis(400),
            write: Duration::from_secs(5),
        }
    }

    /// Binds a listener on an ephemeral port (loopback unless `bind` says
    /// otherwise) and runs the full server (sampler + accept loop) against
    /// it. Returns the bound address, the shared server (test accessors),
    /// the stop flag and the accept-loop thread handle.
    fn start_test_server(
        auth: Option<&str>,
    ) -> (
        SocketAddr,
        Arc<Server>,
        Arc<AtomicBool>,
        thread::JoinHandle<()>,
    ) {
        start_test_server_on("127.0.0.1", auth)
    }

    /// Like [`start_test_server`] but with an explicit bind address, so the
    /// wildcard (`0.0.0.0`) LAN path is exercised the same way loopback is.
    fn start_test_server_on(
        bind: &str,
        auth: Option<&str>,
    ) -> (
        SocketAddr,
        Arc<Server>,
        Arc<AtomicBool>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind((bind, 0)).expect("bind test listener");
        let local = listener.local_addr().expect("local addr");
        let cfg = ServeConfig {
            bind: bind.to_string(),
            port: local.port(),
            interval_secs: 0.05,
            max_procs: 20,
            token: auth.map(str::to_string),
            kiosk: false,
        };
        let server = Arc::new(
            Server::start_with_timeouts(&cfg, test_timeouts()).expect("start test server"),
        );
        let stop = Arc::clone(&server.stop);
        let accept = Arc::clone(&server);
        let handle = thread::spawn(move || accept.accept_loop(listener));
        (local, server, stop, handle)
    }

    fn tcp_connect(addr: SocketAddr) -> TcpStream {
        TcpStream::connect(addr).expect("tcp connect")
    }

    struct HttpResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl HttpResponse {
        #[allow(dead_code)]
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// Parses a buffered head; returns `(body_start, content_length, head)`.
    fn parse_head(buf: &[u8]) -> Option<(usize, usize, String)> {
        let p = find_terminator(buf)?;
        let head = String::from_utf8_lossy(&buf[..p]).into_owned();
        let cl = head
            .lines()
            .find_map(|l| {
                let lower = l.trim().to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        Some((p + 4, cl, head))
    }

    /// Parses the first complete HTTP response out of `buf`, returning it
    /// together with whatever trailing bytes follow it. `None` when `buf`
    /// holds only part of a response (a head without a terminator, or a body
    /// that isn't fully buffered yet). Shared by the single-response reader
    /// and the pipelined reader, so both slice responses identically.
    fn try_split_response(buf: &[u8]) -> Option<(HttpResponse, Vec<u8>)> {
        let (body_start, content_length, head) = parse_head(buf)?;
        let end = body_start + content_length;
        if buf.len() < end {
            return None;
        }
        let body = buf[body_start..end].to_vec();
        let status = head
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let headers = head
            .lines()
            .skip(1)
            .filter_map(|l| {
                let mut it = l.splitn(2, ':');
                let k = it.next()?.trim().to_string();
                let v = it.next()?.trim().to_string();
                Some((k, v))
            })
            .collect();
        Some((
            HttpResponse {
                status,
                headers,
                body,
            },
            buf[end..].to_vec(),
        ))
    }

    /// Reads one HTTP/1.1 response (head + body per Content-Length) from a
    /// connection. For a single response per connection: a socket read may
    /// return bytes beyond this response, and those are discarded here.
    /// Pipelined keep-alive traffic must use [`PipelinedReader`], which
    /// retains the residue for the next response.
    fn read_one_response(stream: &mut TcpStream) -> HttpResponse {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            if let Some((resp, _)) = try_split_response(&buf) {
                return resp;
            }
            let mut chunk = [0u8; 8192];
            let n = stream
                .read(&mut chunk)
                .unwrap_or_else(|e| panic!("read failed while reading response: {e}"));
            assert!(n > 0, "connection closed mid-response");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Fallible variant used when a connection may be refused (503) or simply
    /// closed by the server; returns `None` on EOF or timeout.
    fn try_read_one_response(stream: &mut TcpStream) -> Option<HttpResponse> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            if let Some((resp, _)) = try_split_response(&buf) {
                return Some(resp);
            }
            let mut chunk = [0u8; 8192];
            match stream.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => return None,
            }
        }
    }

    /// Reads multiple keep-alive responses off a single socket. A socket read
    /// can return several pipelined responses in one chunk, so the residue
    /// beyond the just-parsed response is retained for the next
    /// [`next`](Self::next) call instead of being dropped.
    struct PipelinedReader {
        stream: TcpStream,
        pending: Vec<u8>,
    }

    impl PipelinedReader {
        fn new(stream: TcpStream) -> Self {
            Self {
                stream,
                pending: Vec::new(),
            }
        }

        fn next(&mut self) -> HttpResponse {
            loop {
                if let Some((resp, leftover)) = try_split_response(&self.pending) {
                    self.pending = leftover;
                    return resp;
                }
                let mut chunk = [0u8; 8192];
                let n = self
                    .stream
                    .read(&mut chunk)
                    .unwrap_or_else(|e| panic!("read failed while reading response: {e}"));
                assert!(n > 0, "connection closed mid-response");
                self.pending.extend_from_slice(&chunk[..n]);
            }
        }
    }

    fn http_get(addr: SocketAddr, target: &str) -> HttpResponse {
        let mut s = tcp_connect(addr);
        let _ = write!(
            s,
            "GET {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
        );
        let _ = s.flush();
        read_one_response(&mut s)
    }

    /// Like `http_get` but with an extra header line (e.g. a Bearer auth
    /// header) inserted before `Connection:`.
    fn http_get_with_auth(addr: SocketAddr, target: &str, auth_header: &str) -> HttpResponse {
        let mut s = tcp_connect(addr);
        let _ = write!(
            s,
            "GET {target} HTTP/1.1\r\nHost: t\r\n{auth_header}\r\nConnection: close\r\n\r\n"
        );
        let _ = s.flush();
        read_one_response(&mut s)
    }

    fn wait_until(what: &str, cap: Duration, mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while start.elapsed() < cap {
            if cond() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out after {cap:?} waiting for {what}");
    }

    /// Waits until the sampler holds a live payload — and if it went idle
    /// first (warm-up build outlasting the test `idle_grace` on a loaded
    /// host), re-arms it with a dashboard request instead of deadlocking on a
    /// payload that was built and then dropped. With a token configured the
    /// re-arm requests must authenticate, or the sampler never comes back.
    /// Any payload-request eventually wins, so the wait is robust to the test
    /// grace being shorter than the build.
    fn wait_for_payload(addr: SocketAddr, server: &Server, auth: Option<&str>, what: &str) {
        let start = Instant::now();
        loop {
            if server.has_payload() {
                return;
            }
            if start.elapsed() > Duration::from_secs(15) {
                panic!("timed out waiting for {what}");
            }
            // Re-arm sampling if the payload was dropped mid-wait. 503 during
            // a rebuild is fine — the next pass just sends another request.
            match auth {
                Some(tok) => {
                    let _ = http_get_with_auth(
                        addr,
                        "/api/snapshot",
                        &format!("Authorization: Bearer {tok}"),
                    );
                }
                None => {
                    let _ = http_get(addr, "/api/snapshot");
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Polls `target` until it answers 200, re-arming the sampler on every
    /// attempt. A wake request after idle may briefly 503 while the WebState
    /// rebuilds — that is expected; the first live 200 wins, so a test that
    /// needs a fresh payload right after waking never races the rebuild. A
    /// sampler that genuinely failed to re-arm panics on the cap instead.
    fn wait_for_200(
        addr: SocketAddr,
        target: &str,
        auth: Option<&str>,
        what: &str,
    ) -> HttpResponse {
        let start = Instant::now();
        loop {
            let resp = match auth {
                Some(tok) => {
                    http_get_with_auth(addr, target, &format!("Authorization: Bearer {tok}"))
                }
                None => http_get(addr, target),
            };
            if resp.status == 200 {
                return resp;
            }
            if start.elapsed() > Duration::from_secs(15) {
                panic!("timed out waiting for {what} (last status {})", resp.status);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Parses a snapshot response, failing loudly on non-200.
    fn json_snapshot(resp: &HttpResponse) -> serde_json::Value {
        assert_eq!(
            resp.status,
            200,
            "expected 200, got {} (body: {:?})",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        );
        serde_json::from_slice(&resp.body).expect("snapshot is valid JSON")
    }

    /// Deterministic, fully-populated snapshot used by serialization tests.
    fn sample_snapshot() -> crate::model::WebSnapshot {
        let mut history = crate::model::WebHistory {
            cpu: vec![1.0, 2.0],
            mem: vec![3.0, 4.0],
            swap: vec![0.0, 0.0],
            rx: vec![5.0, 6.0],
            tx: vec![7.0, 8.0],
            cores: vec![vec![1.0, 2.0]],
            disks: std::collections::BTreeMap::new(),
        };
        history.disks.insert("C:".to_string(), vec![1.0, 2.0]);
        crate::model::WebSnapshot {
            ts: 1_720_000_000,
            system: crate::model::SystemInfo {
                hostname: Some("host".to_string()),
                os_name: Some("linux".to_string()),
                os_version: Some("1.0".to_string()),
                kernel_version: Some("6.1".to_string()),
                arch: "x86_64".to_string(),
                uptime_secs: 100,
                load_average: Some(crate::model::LoadAvgInfo {
                    one: 0.1,
                    five: 0.2,
                    fifteen: 0.3,
                }),
            },
            cpu: crate::model::CpuInfo {
                model: "cpu".to_string(),
                physical_cores: Some(4),
                logical_cores: 4,
                usage: 12.5,
                cores: vec![crate::model::CpuCoreInfo {
                    name: "cpu0".to_string(),
                    usage: 10.0,
                    frequency_mhz: 3000,
                }],
            },
            memory: crate::model::MemoryInfo {
                total: 1000,
                used: 250,
                available: 750,
                free: 700,
                swap_total: 200,
                swap_used: 10,
                used_percent: 25.0,
            },
            disks: vec![crate::model::WebDiskInfo {
                name: "nvme".to_string(),
                mount_point: "C:".to_string(),
                file_system: "NTFS".to_string(),
                total: 500,
                available: 250,
                used: 250,
                read_bps: 3.0,
                write_bps: 1.5,
                removable: false,
                read_only: false,
            }],
            networks: vec![crate::model::WebNetworkInfo {
                name: "eth0".to_string(),
                received: 99,
                transmitted: 88,
                rx_bps: 2.0,
                tx_bps: 4.0,
            }],
            processes: vec![crate::model::WebProcessInfo {
                pid: 1,
                name: "svc".to_string(),
                user: Some("root".to_string()),
                cpu_usage: 1.0,
                memory: 10,
                memory_percent: 1.0,
                state: "run".to_string(),
                threads: Some(2),
                uptime_secs: 5,
            }],
            sensors: crate::model::SensorsInfo {
                temperatures: vec![crate::model::TemperatureInfo {
                    label: "cpu".to_string(),
                    temperature_c: Some(42.0),
                    max_c: None,
                    critical_c: None,
                }],
            },
            history,
        }
    }

    // --- lifecycle -----------------------------------------------------------

    #[test]
    fn server_warmup_then_idle_then_wake_preserves_history() {
        let (addr, server, _stop, _accept) = start_test_server(None);
        wait_for_payload(addr, &server, None, "first payload");

        // Let the 50 ms sampler accumulate a few history points.
        thread::sleep(Duration::from_millis(400));
        // Poll until a live snapshot answers: under full-suite contention a
        // warm-up build can outlast the test idle_grace and drop a published
        // payload, so the read re-arms instead of racing the rebuild.
        let before = json_snapshot(&wait_for_200(
            addr,
            "/api/snapshot",
            None,
            "snapshot after history accumulation",
        ));
        let before_len = before["history"]["cpu"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        assert!(before_len > 0, "history should have accumulated");
        assert!(
            before["processes"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        );

        // Disconnect: sampler drops state and payload after idle_grace.
        wait_until("sampler goes idle", Duration::from_secs(10), || {
            !server.is_sampling()
        });
        wait_until("payload dropped", Duration::from_secs(10), || {
            !server.has_payload()
        });

        // A fresh dashboard wakes it. The first request may briefly 503 while the
        // WebState rebuilds; poll until a live snapshot answers (every failed
        // attempt re-arms the sampler, so this always progresses), then read
        // the preserved history.
        let _wake = http_get(addr, "/api/snapshot"); // may 503 while rebuilding
        let after = json_snapshot(&wait_for_200(
            addr,
            "/api/snapshot",
            None,
            "snapshot after idle wake",
        ));
        let after_len = after["history"]["cpu"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        assert!(
            after_len >= before_len,
            "history shrank across idle: {before_len} -> {after_len}"
        );
    }

    #[test]
    fn health_check_never_wakes_idle_sampler() {
        let (addr, server, _stop, _accept) = start_test_server(None);
        wait_for_payload(addr, &server, None, "first payload");

        // Health works while live...
        for _ in 0..3 {
            let r = http_get(addr, "/health");
            assert_eq!(r.status, 200);
            assert_eq!(r.body, b"{\"ok\":true}");
        }

        // ...then the server goes idle...
        wait_until("sampler goes idle", Duration::from_secs(10), || {
            !server.is_sampling()
        });
        wait_until("payload dropped", Duration::from_secs(10), || {
            !server.has_payload()
        });

        // ...and health probes must NOT re-arm it.
        let r = http_get(addr, "/health");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"{\"ok\":true}");
        // Health checks never mark the server as watched, so it must stay
        // idle. A tick landing inside the probe may briefly rebuild the state;
        // wait (with a cap) for the sampler to drop again rather than relying
        // on a fixed sleep — terminates as soon as it does and fails loudly if
        // a probe ever permanently armed it.
        wait_until(
            "sampler idle after health probe",
            Duration::from_secs(5),
            || !server.is_sampling() && !server.has_payload(),
        );
    }

    #[test]
    fn metrics_service_live_and_idle() {
        let (addr, server, _stop, _accept) = start_test_server(None);
        wait_for_payload(addr, &server, None, "first payload");

        // Live scrape: poll until a real 200 answers (the very first read can hit
        // the warm-up 503 if a contended build just dropped the payload).
        let r = wait_for_200(addr, "/metrics", None, "live metrics scrape");
        let text = String::from_utf8_lossy(&r.body).into_owned();
        assert!(text.contains("sysview_cpu_usage_percent"), "{text}");
        assert!(text.contains("sysview_processes"), "{text}");

        // Scraping a sleeping server re-arms sampling, like a dashboard poll.
        wait_until("sampler goes idle", Duration::from_secs(10), || {
            !server.is_sampling()
        });
        wait_until("payload dropped", Duration::from_secs(10), || {
            !server.has_payload()
        });
        let r = http_get(addr, "/metrics"); // may be 503 while warming up
        assert!(
            r.status == 200 || r.status == 503,
            "expected 200 or 503 while waking, got {}",
            r.status
        );
        // Poll until a live scrape answers (re-arming on every attempt); the
        // first wake scrape can 503 while the WebState rebuilds.
        let r2 = wait_for_200(addr, "/metrics", None, "metrics after idle wake");
        assert!(String::from_utf8_lossy(&r2.body).contains("sysview_processes"));
    }

    // --- auth ----------------------------------------------------------------

    #[test]
    fn auth_accepts_query_and_bearer_tokens() {
        let (addr, server, _stop, _accept) = start_test_server(Some("s3cret"));
        wait_for_payload(addr, &server, Some("s3cret"), "first payload");

        assert_eq!(http_get(addr, "/api/snapshot").status, 401);
        assert_eq!(http_get(addr, "/api/snapshot?token=nope").status, 401);
        assert_eq!(http_get(addr, "/api/snapshot?token=").status, 401);
        // Valid credentials must eventually serve live data, but the first
        // request after a short pause can 503 while warming up, so poll.
        assert_eq!(
            wait_for_200(addr, "/api/snapshot?token=s3cret", None, "authed snapshot").status,
            200
        );
        assert_eq!(
            wait_for_200(
                addr,
                "/api/snapshot",
                Some("s3cret"),
                "authed snapshot via bearer"
            )
            .status,
            200
        );
        assert_eq!(
            http_get_with_auth(addr, "/api/snapshot", "Authorization: Bearer nope").status,
            401
        );
        // Scheme prefix is case-insensitive.
        assert_eq!(
            wait_for_200(
                addr,
                "/api/snapshot",
                Some("s3cret"),
                "authed snapshot via lowercase bearer"
            )
            .status,
            200
        );
    }

    #[test]
    fn auth_gate_applies_to_every_route() {
        let (addr, server, _stop, _accept) = start_test_server(Some("s3cret"));
        wait_for_payload(addr, &server, Some("s3cret"), "first payload");
        for target in [
            "/",
            "/index.html",
            "/api/snapshot",
            "/metrics",
            "/health",
            "/favicon.ico",
            "/nope",
        ] {
            let r = http_get(addr, target);
            assert_eq!(r.status, 401, "route {target} leaked without a token");
            let r = http_get(addr, &format!("{target}?token=s3cret"));
            assert_ne!(r.status, 401, "route {target} rejected a valid token");
        }
    }

    // --- robustness ----------------------------------------------------------

    #[test]
    fn malformed_input_is_rejected_and_server_survives() {
        let (addr, server, _stop, _accept) = start_test_server(None);
        wait_for_payload(addr, &server, None, "first payload");

        let cases: &[(&str, u16)] = &[
            ("GARBAGE\r\n\r\n", 405),
            ("POST /api/snapshot HTTP/1.1\r\n\r\n", 405),
            ("GET http://evil.example/ HTTP/1.1\r\n\r\n", 400),
            ("GET /../etc/passwd HTTP/1.1\r\n\r\n", 400),
            ("GET /%2e%2e/etc/passwd HTTP/1.1\r\n\r\n", 400),
            ("GET /api/snapshot HTTP/1.1 EXTRA\r\n\r\n", 400),
        ];
        for (req, want) in cases {
            let mut s = tcp_connect(addr);
            s.write_all(req.as_bytes()).unwrap();
            s.flush().unwrap();
            let resp = read_one_response(&mut s);
            assert_eq!(resp.status, *want, "for request {req:?}");
        }

        // An oversized (>= 16 KB) unterminated head must be bounded, not
        // buffered without limit. The first line still parses, so it is served
        // — the point is the server survives and keeps a sane cap.
        let mut s = tcp_connect(addr);
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: t\r\nX-Pad: ")
            .unwrap();
        s.write_all(&[b'a'; 20 * 1024]).unwrap();
        s.flush().unwrap();
        let resp = read_one_response(&mut s);
        assert_eq!(resp.status, 200, "bounded head should still be served");

        // Abandoned / half-sent connections must release their threads.
        for _ in 0..8 {
            let mut s = tcp_connect(addr);
            let _ = s.write_all(b"GET /api/snap"); // partial head, then drop
            drop(s);
        }
        wait_until("connections released", Duration::from_secs(10), || {
            server.conns() == 0
        });

        // The server survived everything: a normal request still works.
        assert_eq!(http_get(addr, "/").status, 200);
    }

    #[test]
    fn max_conns_is_enforced_and_recovers() {
        let (addr, server, _stop, _accept) = start_test_server(None);
        wait_for_payload(addr, &server, None, "first payload");

        // Occupy every slot. A connection that holds a partial request (no
        // terminator, so the server parks a handler thread in read) would
        // still expire after the read timeout — racing the time it takes to
        // fill all MAX_CONNS slots through the accept loop's 5 ms poll under
        // parallel-suite load and making this test flaky. Instead each holder
        // drips non-terminating pad bytes, so its handler never goes quiet
        // (never hits read timeout) yet never completes a head: the hold is
        // deterministic for as long as the test needs.
        let release = Arc::new(AtomicBool::new(false));
        let mut holders: Vec<thread::JoinHandle<()>> = Vec::new();
        for _ in 0..MAX_CONNS {
            let release = Arc::clone(&release);
            holders.push(thread::spawn(move || {
                let mut s = tcp_connect(addr);
                let _ = write!(s, "GET /api/snapshot HTTP/1.1\r\nHost: t\r\nX-Pad: ");
                let pad = [0u8; 64];
                while !release.load(Ordering::Relaxed) {
                    if s.write_all(&pad).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(60));
                }
            }));
        }
        wait_until("all slots occupied", Duration::from_secs(10), || {
            server.conns() == MAX_CONNS
        });

        // Excess connections must be refused while every slot is held. Two
        // sides are asserted:
        //   * server side: the reject path runs (a test-only refusal counter
        //     increments for each excess connection) — this never depends on
        //     the OS delivering the 503 body to the client;
        //   * client side: no probe is ever served a 200 (a slot was wrongly
        //     freed). Note: on this host the server's freshly-written 503 is
        //     occasionally lost when the accept-loop thread closes right after
        //     writing (the client sees a fast EOF instead), so the body read
        //     is best-effort only — sometimes it is the 503 itself.
        let refused_before = server.rejected();
        for _ in 0..16 {
            let mut s = tcp_connect(addr);
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let _ = write!(
                s,
                "GET /api/snapshot HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
            );
            let _ = s.flush();
            match try_read_one_response(&mut s) {
                Some(resp) if resp.status == 503 => {} // explicit refusal read
                Some(resp) if resp.status == 200 => {
                    panic!("excess connection was served a 200 while all slots were held")
                }
                Some(resp) => panic!("unexpected status {} for excess connection", resp.status),
                None => {} // fast EOF: refusal body lost on the wire (see comment above)
            }
            drop(s);
        }
        assert!(
            server.rejected() > refused_before,
            "server never ran the 503 rejection path despite full slots"
        );

        // Release the holders: they stop dripping and close, the handlers see
        // EOF and release their slots, and the server fully recovers.
        release.store(true, Ordering::Relaxed);
        for h in holders {
            let _ = h.join();
        }
        wait_until("all slots released", Duration::from_secs(10), || {
            server.conns() == 0
        });

        // Server fully recovered.
        assert_eq!(http_get(addr, "/").status, 200);
    }

    #[test]
    fn quiet_connections_are_released_by_read_timeout() {
        let (addr, server, _stop, _accept) = start_test_server(None);
        wait_for_payload(addr, &server, None, "first payload");

        // A connection that sends a partial head and then goes quiet parks a
        // handler thread — which the read timeout must release.
        let mut s = tcp_connect(addr);
        s.write_all(b"GET / HTTP/1.1\r\nHost: t\r\nX-Pad: ")
            .unwrap();
        s.flush().unwrap();
        wait_until("connection accepted", Duration::from_secs(10), || {
            server.conns() == 1
        });
        wait_until("quiet connection released", Duration::from_secs(10), || {
            server.conns() == 0
        });

        // The server closed the connection as part of the release.
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut byte = [0u8; 1];
        let n = s.read(&mut byte).unwrap_or(0);
        assert_eq!(n, 0, "server should close a timed-out connection");

        // Server is still healthy.
        assert_eq!(http_get(addr, "/health").status, 200);
    }

    #[test]
    fn keep_alive_reuses_the_connection() {
        let (addr, server, _stop, _accept) = start_test_server(None);
        wait_for_payload(addr, &server, None, "first payload");

        // Two pipelined keep-alive requests in one write. A socket read can
        // return both responses in a single chunk, so read them through the
        // residue-preserving reader (the naive per-response read would drop
        // the bytes it over-read and misparse the second response — exactly
        // what happened on Linux, where reads coalesce).
        let mut reader = PipelinedReader::new(tcp_connect(addr));
        reader
            .stream
            .write_all(
                b"GET /health HTTP/1.1\r\nHost: t\r\n\r\nGET /metrics HTTP/1.1\r\nHost: t\r\n\r\n",
            )
            .unwrap();
        reader.stream.flush().unwrap();
        let r1 = reader.next();
        assert_eq!(r1.status, 200);
        assert_eq!(r1.header("connection"), Some("keep-alive"));
        assert_eq!(r1.body, b"{\"ok\":true}");
        let r2 = reader.next();
        assert_eq!(r2.status, 200);
        assert!(r2.body.windows(8).any(|w| w == &b"sysview_"[..]));

        // A third request on the same socket confirms the connection stayed up.
        reader
            .stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: t\r\n\r\n")
            .unwrap();
        reader.stream.flush().unwrap();
        let r3 = reader.next();
        assert_eq!(r3.status, 200);

        // Connection: close terminates the connection after one final response.
        reader
            .stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .unwrap();
        reader.stream.flush().unwrap();
        let r4 = reader.next();
        assert_eq!(r4.status, 200);
        let mut s = reader.stream;
        let mut byte = [0u8; 1];
        let n = s.read(&mut byte).unwrap_or(0);
        assert_eq!(n, 0, "server should close after Connection: close");
    }

    // --- history ring --------------------------------------------------------

    #[test]
    fn history_cores_restart_on_core_count_change() {
        let mut ring = HistoryRing::new();
        for i in 0..(HISTORY_MAX + 10) {
            ring.push_cores(&[i as f32, i as f32, i as f32]);
        }
        let w = ring.to_web();
        assert_eq!(w.cores.len(), 3);
        assert_eq!(w.cores[0].len(), HISTORY_MAX);

        // The VM shrinks to two cores: the per-core windows restart cleanly
        // instead of misaligning series across a resized core set.
        for i in 0..5 {
            ring.push_cores(&[10.0 + i as f32, 20.0 + i as f32]);
        }
        let w2 = ring.to_web();
        assert_eq!(w2.cores.len(), 2);
        assert_eq!(w2.cores[0].len(), 5);
        assert_eq!(w2.cores[0][0], 10.0);
        assert_eq!(w2.cores[1][4], 24.0);
    }

    // --- /metrics ------------------------------------------------------------

    #[test]
    fn metrics_escapes_hostile_label_values() {
        // Decoded label text: a disk mount containing a backslash, a quote and
        // a real newline; an interface with a quote+backslash; a sensor label
        // with a newline; and a dead sensor that must emit nothing.
        let json = r#"{
            "ts": 1,
            "system": {"uptime_secs": 1, "load_average": null},
            "cpu": {"usage": 1.0, "logical_cores": 1},
            "memory": {"total": 1, "used": 0, "available": 1, "free": 1, "used_percent": 0.0, "swap_total": 0, "swap_used": 0},
            "disks": [{"mount_point": "C:\\foo\"bar\nbaz", "total": 100, "used": 50, "read_bps": 1.0, "write_bps": 2.0}],
            "networks": [{"name": "eth\"0\\", "received": 1, "transmitted": 2, "rx_bps": 3.0, "tx_bps": 4.0}],
            "processes": [{}],
            "sensors": {"temperatures": [{"label": "cpu\n0", "temperature_c": 42.0}, {"label": "dead", "temperature_c": null}]}
        }"#;
        let out = json_to_metrics(json.as_bytes());
        assert!(out.contains("mount=\"C:\\\\foo\\\"bar\\nbaz\""), "{out}");
        assert!(out.contains("interface=\"eth\\\"0\\\\\""), "{out}");
        assert!(
            out.contains("sysview_sensor_temperature_celsius{sensor=\"cpu\\n0\"} 42"),
            "{out}"
        );
        // A real newline must never appear inside a label value.
        assert!(!out.contains("bar\nbaz"), "{out}");
        assert!(!out.contains("cpu\n0"), "{out}");
        // A sensor without a reading produces no gauge line.
        assert!(!out.contains("sensor=\"dead\""), "{out}");
    }

    #[test]
    fn metrics_counts_processes_without_materializing_them() {
        // The process array decode type is zero-sized: serde counts elements
        // without allocating anything per process.
        assert_eq!(std::mem::size_of::<MetricsIgnored>(), 0);
        let json = r#"{
            "ts": 1,
            "system": {"uptime_secs": 1, "load_average": null},
            "cpu": {"usage": 1.0, "logical_cores": 1},
            "memory": {"total": 1, "used": 0, "available": 1, "free": 1, "used_percent": 0.0, "swap_total": 0, "swap_used": 0},
            "disks": [],
            "networks": [],
            "processes": [{}, {}, {}, {"pid": 9, "name": "x"}],
            "sensors": {"temperatures": []}
        }"#;
        let out = json_to_metrics(json.as_bytes());
        assert!(out.contains("sysview_processes 4\n"), "{out}");
    }

    #[test]
    fn metrics_conversion_stays_cheap_for_large_snapshots() {
        // Build a snapshot with 3000 processes, 100 disks and 100 interfaces.
        let mut json = String::with_capacity(512 * 1024);
        json.push_str(
            "{\"ts\":1,\"system\":{\"uptime_secs\":1,\"load_average\":null},\
             \"cpu\":{\"usage\":1.0,\"logical_cores\":4},\
             \"memory\":{\"total\":1,\"used\":0,\"available\":1,\"free\":1,\"used_percent\":0.0,\"swap_total\":0,\"swap_used\":0},\
             \"disks\":[",
        );
        for i in 0..100 {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                "{{\"mount_point\":\"/mnt/{i}\",\"total\":{i},\"used\":0,\"read_bps\":0.0,\"write_bps\":0.0}}"
            ));
        }
        json.push_str("],\"networks\":[");
        for i in 0..100 {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                "{{\"name\":\"eth{i}\",\"received\":0,\"transmitted\":0,\"rx_bps\":0.0,\"tx_bps\":0.0}}"
            ));
        }
        json.push_str("],\"processes\":[");
        for i in 0..3000 {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                "{{\"pid\":{i},\"name\":\"p{i}\",\"user\":null,\"cpu_usage\":0.0,\"memory\":0,\"memory_percent\":0.0,\"state\":\"run\",\"threads\":null,\"uptime_secs\":1}}"
            ));
        }
        json.push_str("],\"sensors\":{\"temperatures\":[]}}");
        let bytes = json.as_bytes();

        let start = Instant::now();
        let mut out_len = 0usize;
        for _ in 0..5 {
            out_len += json_to_metrics(bytes).len();
        }
        let elapsed = start.elapsed();
        assert!(out_len > 1000, "conversion produced nothing?");
        // Guard, not benchmark: measured 70-115 ms for 5 conversions of this
        // 3000-process snapshot (debug build, Windows), so 2 s of headroom is
        // ~20x the worst observed run. Generous enough to never flake on a
        // loaded host, tight enough to catch a regression into quadratic or
        // per-process-allocation conversion (which would land an order of
        // magnitude above this).
        assert!(
            elapsed < Duration::from_secs(2),
            "conversion too slow: {elapsed:?}"
        );
    }

    // --- perf ----------------------------------------------------------------

    #[test]
    fn snapshot_serialize_reuses_buffer_capacity() {
        // The sampler recycles one buffer per session: it takes the cached
        // Vec, clears it and writes the new snapshot in place. Clearing a Vec
        // preserves capacity, so successive ticks must never reallocate.
        // This pins that pattern deterministically.
        let mut buf = Vec::new();
        serde_json::to_writer(&mut buf, &sample_snapshot()).unwrap();
        let first = buf.len();
        let cap = buf.capacity();
        assert!(first > 0);

        buf.clear();
        serde_json::to_writer(&mut buf, &sample_snapshot()).unwrap();
        assert!(!buf.is_empty());
        assert_eq!(
            buf.capacity(),
            cap,
            "buffer capacity must be reused across ticks"
        );
        assert_eq!(
            buf.len(),
            first,
            "same-size snapshot serializes to same length"
        );
    }

    // --- unit ----------------------------------------------------------------

    #[test]
    fn timeouts_default_are_production_constants() {
        let t = Timeouts::default();
        assert_eq!(t.idle_grace, IDLE_GRACE);
        assert_eq!(t.read, READ_TIMEOUT);
        assert_eq!(t.write, WRITE_TIMEOUT);
    }

    /// Memory posture guard: connection threads run on a small capped stack
    /// (head parsing and response writes are shallow; even the /metrics JSON
    /// decode is bounded-depth), so the worst case across all MAX_CONNS slots
    /// is a fixed, bounded commit instead of the ~1-2 MiB-per-thread default
    /// reserve. This pins that ceiling: raise the bound only if a handler
    /// ever starts doing deep or unbounded work on the connection thread.
    #[test]
    fn connection_thread_stacks_are_bounded() {
        // black_box keeps the values opaque so the assertions genuinely run
        // (and guard future edits to these constants) instead of being folded
        // away as compile-time-known truth.
        let stack = std::hint::black_box(CONN_STACK_SIZE);
        let conns = std::hint::black_box(MAX_CONNS);
        assert!(
            stack >= 32 * 1024,
            "stack of {stack} is too small to be sane"
        );
        let worst_case = conns * stack;
        assert!(
            worst_case <= 32 * 1024 * 1024,
            "connection threads could commit {worst_case} bytes"
        );
    }
}
