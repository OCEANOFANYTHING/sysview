use serde::Serialize;
use std::collections::BTreeMap;

/// Full snapshot used by the default `sysview` overview.
#[derive(Serialize)]
pub struct Overview {
    pub system: SystemInfo,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub disks: Vec<DiskInfo>,
    pub networks: Vec<NetworkInfo>,
    pub processes: Vec<ProcessInfo>,
}

#[derive(Serialize)]
pub struct SystemInfo {
    pub hostname: Option<String>,
    pub os_name: Option<String>,
    pub os_version: Option<String>,
    pub kernel_version: Option<String>,
    pub arch: String,
    pub uptime_secs: u64,
    /// Load average is not available on Windows (always `None` there).
    pub load_average: Option<LoadAvgInfo>,
}

#[derive(Serialize)]
pub struct LoadAvgInfo {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

#[derive(Serialize)]
pub struct CpuInfo {
    pub model: String,
    pub physical_cores: Option<usize>,
    pub logical_cores: usize,
    /// Overall CPU usage in percent (0-100).
    pub usage: f32,
    pub cores: Vec<CpuCoreInfo>,
}

#[derive(Serialize)]
pub struct CpuCoreInfo {
    pub name: String,
    pub usage: f32,
    pub frequency_mhz: u64,
}

#[derive(Serialize)]
pub struct MemoryInfo {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub free: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// Used percent (0-100).
    pub used_percent: f32,
}

#[derive(Serialize)]
pub struct DiskInfo {
    pub name: String,
    pub mount_point: String,
    pub file_system: String,
    pub total: u64,
    pub available: u64,
    pub used: u64,
    /// Bytes read/written since the previous refresh (0 on first sample).
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub removable: bool,
    pub read_only: bool,
}

#[derive(Serialize)]
pub struct NetworkInfo {
    pub name: String,
    /// Total bytes received since boot.
    pub received: u64,
    /// Total bytes transmitted since boot.
    pub transmitted: u64,
}

#[derive(Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub cpu_usage: f32,
    /// Resident memory in bytes.
    pub memory: u64,
    /// Memory percent of total RAM (0-100).
    pub memory_percent: f32,
    /// Owning user name when resolvable.
    pub user: Option<String>,
    /// Start time as UNIX epoch seconds.
    pub start_time_secs: u64,
    /// Process uptime in seconds.
    pub uptime_secs: u64,
    /// Parent PID when known.
    pub parent: Option<u32>,
    /// Thread/task count when known.
    pub threads: Option<usize>,
    /// Human-readable process state (e.g. "run", "sleep").
    pub state: String,
    /// Command line when available.
    pub cmdline: Option<String>,
}

/// vmstat-style one-line summary.
#[derive(Serialize)]
pub struct VmstatInfo {
    /// Runnable + blocked process counts (platform dependent).
    pub procs_running: usize,
    pub procs_blocked: usize,
    pub memory_used: u64,
    pub memory_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub cpu_usage: f32,
    pub uptime_secs: u64,
}

#[derive(Serialize)]
pub struct SensorsInfo {
    pub temperatures: Vec<TemperatureInfo>,
}

#[derive(Serialize)]
pub struct TemperatureInfo {
    pub label: String,
    /// Current temperature in Celsius, when reported.
    pub temperature_c: Option<f32>,
    pub max_c: Option<f32>,
    pub critical_c: Option<f32>,
}

/// Snapshot served by the embedded web dashboard. Deliberately leaner than
/// `Overview`: no command lines, and it carries per-second I/O rates so the
/// dashboard can draw sparklines without keeping state itself.
#[derive(Serialize)]
pub struct WebSnapshot {
    /// UNIX epoch seconds when the sample was taken.
    pub ts: u64,
    pub system: SystemInfo,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub disks: Vec<WebDiskInfo>,
    pub networks: Vec<WebNetworkInfo>,
    pub processes: Vec<WebProcessInfo>,
    pub sensors: SensorsInfo,
    /// Rolling history maintained by the server so every client (and every
    /// reload) sees the same charts, not a history that starts from zero.
    pub history: WebHistory,
}

/// Rolling per-metric history kept server-side. Indexed in time order; the
/// newest sample is the last element. Every dashboard fetches the same
/// server-generated arrays, so all devices share identical charts.
#[derive(Serialize)]
pub struct WebHistory {
    pub cpu: Vec<f32>,
    pub mem: Vec<f32>,
    pub swap: Vec<f32>,
    pub rx: Vec<f64>,
    pub tx: Vec<f64>,
    /// Per-core CPU usage history; outer index = logical core, inner = time.
    /// Costs `cores x 240 x 4B` (~15 KB for 16 cores) — intentionally small.
    pub cores: Vec<Vec<f32>>,
    /// Per-mount-point disk usage (percent) history, keyed by mount path.
    /// Costs `mounts x 240 x 4B` (a few KB for typical hosts).
    pub disks: BTreeMap<String, Vec<f32>>,
}

/// Disk record for the dashboard with per-second I/O rates.
#[derive(Serialize)]
pub struct WebDiskInfo {
    pub name: String,
    pub mount_point: String,
    pub file_system: String,
    pub total: u64,
    pub available: u64,
    pub used: u64,
    /// Bytes per second read since the previous snapshot.
    pub read_bps: f64,
    /// Bytes per second written since the previous snapshot.
    pub write_bps: f64,
    pub removable: bool,
    pub read_only: bool,
}

#[derive(Serialize)]
pub struct WebNetworkInfo {
    pub name: String,
    /// Total bytes received since boot.
    pub received: u64,
    /// Total bytes transmitted since boot.
    pub transmitted: u64,
    /// Bytes per second since the previous snapshot.
    pub rx_bps: f64,
    /// Bytes per second since the previous snapshot.
    pub tx_bps: f64,
}

/// Lean process record for the dashboard (no cmdline, no parent).
#[derive(Serialize)]
pub struct WebProcessInfo {
    pub pid: u32,
    pub name: String,
    pub user: Option<String>,
    pub cpu_usage: f32,
    pub memory: u64,
    pub memory_percent: f32,
    pub state: String,
    pub threads: Option<usize>,
    pub uptime_secs: u64,
}
