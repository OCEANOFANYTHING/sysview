use std::collections::HashMap;
use std::thread;

#[cfg(target_os = "linux")]
use std::net::Ipv6Addr;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

use sysinfo::{
    Components, Disks, MINIMUM_CPU_UPDATE_INTERVAL, Networks, Process, ProcessRefreshKind,
    ProcessesToUpdate, System, Uid, UpdateKind, Users,
};

use crate::model::*;
use crate::{Cli, SortKey};

/// Number of samples the embedded dashboard keeps per metric. This is the
/// shared, server-side history window: every client sees the same 240 points.
pub const HISTORY_MAX: usize = 240;

/// Collects system-wide info. Uses static sysinfo accessors, so no
/// `System` instance is needed.
pub fn system_info() -> SystemInfo {
    #[cfg(unix)]
    let load_average = {
        let la = System::load_average();
        Some(LoadAvgInfo {
            one: la.one,
            five: la.five,
            fifteen: la.fifteen,
        })
    };
    #[cfg(not(unix))]
    let load_average = None; // load average is not implemented on Windows

    SystemInfo {
        hostname: System::host_name(),
        os_name: System::name(),
        os_version: System::os_version(),
        kernel_version: System::kernel_version(),
        arch: std::env::consts::ARCH.to_string(),
        uptime_secs: System::uptime(),
        load_average,
    }
}

/// Samples CPU usage twice (sysinfo computes usage as a diff) and returns
/// the resulting info. Leaves `sys` refreshed with CPU data.
pub fn cpu(sys: &mut System) -> CpuInfo {
    sample_cpu(sys);
    build_cpu(sys)
}

/// Refreshes memory counters.
pub fn memory(sys: &mut System) -> MemoryInfo {
    sys.refresh_memory();
    build_memory(sys)
}

pub fn disks() -> Vec<DiskInfo> {
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .map(|d| {
            let total = d.total_space();
            let available = d.available_space();
            let usage = d.usage();
            DiskInfo {
                name: {
                    let n = d.name().to_string_lossy();
                    if n.is_empty() {
                        "(no label)".to_string()
                    } else {
                        n.into_owned()
                    }
                },
                mount_point: d.mount_point().to_string_lossy().into_owned(),
                file_system: d.file_system().to_string_lossy().into_owned(),
                total,
                available,
                used: total.saturating_sub(available),
                read_bytes: usage.read_bytes,
                write_bytes: usage.written_bytes,
                removable: d.is_removable(),
                read_only: d.is_read_only(),
            }
        })
        .collect()
}

pub fn networks() -> Vec<NetworkInfo> {
    let networks = Networks::new_with_refreshed_list();
    let mut infos: Vec<NetworkInfo> = networks
        .list()
        .iter()
        .map(|(name, data)| NetworkInfo {
            name: name.clone(),
            received: data.total_received(),
            transmitted: data.total_transmitted(),
        })
        .collect();
    infos.sort_by(|a, b| a.name.cmp(&b.name));
    infos
}

/// Samples process CPU usage twice, then returns processes sorted and
/// filtered according to the CLI options.
pub fn processes_filtered(sys: &mut System, cli: &Cli) -> Vec<ProcessInfo> {
    sample_cpu_and_processes(sys);
    sys.refresh_memory();
    build_processes(sys, cli, &uid_name_map())
}

/// Full overview: one sampling pass feeds CPU, processes and memory.
pub fn overview_with(cli: &Cli, sys: &mut System) -> Overview {
    sample_cpu_and_processes(sys);
    sys.refresh_memory();

    Overview {
        system: system_info(),
        cpu: build_cpu(sys),
        memory: build_memory(sys),
        disks: disks(),
        networks: networks(),
        processes: build_processes(sys, cli, &uid_name_map()),
    }
}

/// vmstat-style one-shot summary.
pub fn vmstat(sys: &mut System) -> VmstatInfo {
    sample_cpu_and_processes(sys);
    sys.refresh_memory();

    // Windows reports every process as `Run`, so counting there would just
    // report the process count; only Unix exposes a real run queue.
    let (procs_running, procs_blocked) = if cfg!(windows) {
        (None, None)
    } else {
        let mut running = 0usize;
        let mut blocked = 0usize;
        for p in sys.processes().values() {
            match p.status() {
                sysinfo::ProcessStatus::Run => running += 1,
                sysinfo::ProcessStatus::UninterruptibleDiskSleep => blocked += 1,
                _ => {}
            }
        }
        (Some(running), Some(blocked))
    };

    VmstatInfo {
        procs_running,
        procs_blocked,
        memory_used: sys.used_memory(),
        memory_total: sys.total_memory(),
        swap_used: sys.used_swap(),
        swap_total: sys.total_swap(),
        cpu_usage: sys.global_cpu_usage(),
        uptime_secs: System::uptime(),
    }
}

/// Temperature sensors via sysinfo Components.
pub fn sensors() -> SensorsInfo {
    SensorsInfo {
        temperatures: temperatures_of(&Components::new_with_refreshed_list()),
    }
}

/// The cgroup limits sysview itself runs under. `None` off Linux and on hosts
/// still using cgroup v1, whose layout is different enough to be its own job.
pub fn cgroup() -> Option<CgroupInfo> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        // cgroup v2 has a single entry, `0::/path`; v1 lists one per controller.
        let path = raw
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?
            .trim()
            .to_string();
        let base = PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/'));
        let cores = std::fs::read_to_string(base.join("cpu.max"))
            .ok()
            .and_then(|t| cpu_max_cores(&t));
        let pressure = |name: &str| {
            std::fs::read_to_string(base.join(name))
                .ok()
                .and_then(|t| pressure_avg10(&t))
        };
        Some(CgroupInfo {
            version: 2,
            path,
            cpu_max_cores: cores,
            cpu_usage_secs: std::fs::read_to_string(base.join("cpu.stat"))
                .ok()
                .and_then(|t| stat_value(&t, "usage_usec"))
                .map(|usec| usec as f64 / 1_000_000.0),
            memory_current: counter(&base, "memory.current"),
            memory_max: counter(&base, "memory.max"),
            memory_swap_max: counter(&base, "memory.swap.max"),
            pids_current: counter(&base, "pids.current"),
            pids_max: counter(&base, "pids.max"),
            pressure_cpu: pressure("cpu.pressure"),
            pressure_memory: pressure("memory.pressure"),
            pressure_io: pressure("io.pressure"),
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `cpu.max` is `QUOTA PERIOD` in microseconds, or `max PERIOD` for unlimited.
/// Kept buildable off Linux so the parsing itself stays unit-testable.
#[cfg(any(target_os = "linux", test))]
fn cpu_max_cores(text: &str) -> Option<f64> {
    let mut fields = text.split_whitespace();
    let quota = fields.next()?;
    let period: f64 = fields.next()?.parse().ok()?;
    if quota == "max" || period <= 0.0 {
        return None;
    }
    Some(quota.parse::<f64>().ok()? / period)
}

/// One field out of a `key value` file such as `cpu.stat`.
#[cfg(any(target_os = "linux", test))]
fn stat_value(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// The `avg10` figure out of a PSI file (`some avg10=0.00 avg60=... total=...`).
#[cfg(any(target_os = "linux", test))]
fn pressure_avg10(text: &str) -> Option<f64> {
    text.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))
        .and_then(|v| v.parse().ok())
}

/// A cgroup counter file. `max` (unlimited) reads as absent rather than 0.
#[cfg(target_os = "linux")]
fn counter(base: &Path, name: &str) -> Option<u64> {
    std::fs::read_to_string(base.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod cgroup_parse_tests {
    use super::{cpu_max_cores, pressure_avg10, stat_value};

    #[test]
    fn cpu_max_reads_a_quota_or_unlimited() {
        assert_eq!(cpu_max_cores("200000 100000\n"), Some(2.0));
        assert_eq!(cpu_max_cores("50000 100000"), Some(0.5));
        assert_eq!(cpu_max_cores("max 100000\n"), None);
        assert_eq!(cpu_max_cores("200000 0"), None);
        assert_eq!(cpu_max_cores(""), None);
    }

    #[test]
    fn stat_value_picks_its_key_out_of_a_stat_file() {
        let stat = "usage_usec 1234567\nuser_usec 900000\nsystem_usec 334567\n";
        assert_eq!(stat_value(stat, "usage_usec"), Some(1234567));
        assert_eq!(stat_value(stat, "system_usec"), Some(334567));
        assert_eq!(stat_value(stat, "nope"), None);
    }

    #[test]
    fn pressure_reads_avg10_and_ignores_the_rest() {
        let psi = "some avg10=1.25 avg60=0.40 avg300=0.10 total=99123\n";
        assert_eq!(pressure_avg10(psi), Some(1.25));
        assert_eq!(pressure_avg10("garbage"), None);
    }
}

/// GPUs the kernel exposes through DRM. Only amdgpu and Intel put usage and
/// VRAM in sysfs, so a card driven by the proprietary NVIDIA module shows its
/// identity and driver with no numbers rather than a wrong figure.
pub fn gpus() -> Vec<GpuInfo> {
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
            return Vec::new();
        };
        let mut out: Vec<GpuInfo> = entries
            .flatten()
            .filter_map(|entry| {
                let card = entry.file_name().to_string_lossy().into_owned();
                // `card0` is a device; `card0-DP-1` is a connector.
                if !card.starts_with("card") || card.contains('-') {
                    return None;
                }
                let dev = entry.path().join("device");
                let uevent = std::fs::read_to_string(dev.join("uevent")).ok()?;
                Some(gpu_from(&card, &dev, &uevent))
            })
            .collect();
        out.sort_by(|a, b| a.card.cmp(&b.card));
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn gpu_from(card: &str, dev: &Path, uevent: &str) -> GpuInfo {
    let device_id = uevent_field(uevent, "PCI_ID=").unwrap_or("-").to_string();
    let vendor = device_id
        .split_once(':')
        .map(|(v, _)| match v.to_ascii_uppercase().as_str() {
            "1002" => "AMD",
            "10DE" => "NVIDIA",
            "8086" => "Intel",
            _ => "PCI",
        })
        .unwrap_or("PCI")
        .to_string();
    let number = |name: &str| -> Option<u64> {
        std::fs::read_to_string(dev.join(name))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    GpuInfo {
        card: card.to_string(),
        vendor,
        device_id,
        driver: uevent_field(uevent, "DRIVER=").map(str::to_string),
        vram_total: number("mem_info_vram_total"),
        vram_used: number("mem_info_vram_used"),
        usage_percent: number("gpu_busy_percent").map(|v| v as f32),
        temperature_c: gpu_temp(dev),
    }
}

#[cfg(target_os = "linux")]
fn uevent_field<'a>(uevent: &'a str, key: &str) -> Option<&'a str> {
    uevent.lines().find_map(|line| line.strip_prefix(key))
}

/// The DRM device's first hwmon temperature, in degrees Celsius.
#[cfg(target_os = "linux")]
fn gpu_temp(dev: &Path) -> Option<f32> {
    for entry in std::fs::read_dir(dev.join("hwmon")).ok()?.flatten() {
        let text = std::fs::read_to_string(entry.path().join("temp1_input")).ok()?;
        if let Ok(milli) = text.trim().parse::<f64>() {
            return Some((milli / 1000.0) as f32);
        }
    }
    None
}

/// The handful of live values `sysview check` compares against thresholds.
/// Sampled one-shot into a throwaway `System`, so nothing is retained.
pub struct CheckMetrics {
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub swap_percent: f64,
    /// 1-minute load average; `None` on Windows (not implemented there).
    pub load_one: Option<f64>,
    /// `(mount point, used percent)` for every non-zero-size volume.
    pub disks: Vec<(String, f64)>,
}

/// Samples the values compared by `sysview check`. Deliberately lean: it warms
/// CPU (two samples) and reads memory + disks, never the process table.
pub fn check_metrics() -> CheckMetrics {
    let mut sys = System::new();
    sample_cpu(&mut sys);
    sys.refresh_memory();

    let disks = Disks::new_with_refreshed_list();
    let disks = disks
        .list()
        .iter()
        .filter_map(|d| {
            let total = d.total_space();
            if total == 0 {
                return None;
            }
            let used = total.saturating_sub(d.available_space());
            Some((
                d.mount_point().to_string_lossy().into_owned(),
                used as f64 * 100.0 / total as f64,
            ))
        })
        .collect::<Vec<_>>();

    #[cfg(unix)]
    let load_one = Some(System::load_average().one);
    #[cfg(not(unix))]
    let load_one = None;

    let total_mem = sys.total_memory();
    let total_swap = sys.total_swap();
    CheckMetrics {
        cpu_percent: sys.global_cpu_usage() as f64,
        mem_percent: if total_mem == 0 {
            0.0
        } else {
            sys.used_memory() as f64 * 100.0 / total_mem as f64
        },
        swap_percent: if total_swap == 0 {
            0.0
        } else {
            sys.used_swap() as f64 * 100.0 / total_swap as f64
        },
        load_one,
        disks,
    }
}

/// One socket that is accepting connections (TCP `LISTEN`), parsed from
/// `/proc/net/tcp` and `/proc/net/tcp6`. Linux only; other platforms return an
/// empty list because the OS exposes no equivalent cheaply. The result is
/// bounded by the kernel's listening-socket count, so it stays tiny.
pub fn listening_ports() -> Vec<ListeningPort> {
    #[cfg(target_os = "linux")]
    {
        let mut ports = Vec::new();
        for (file, proto) in [("/proc/net/tcp", "tcp"), ("/proc/net/tcp6", "tcp6")] {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            // Columns: sl local_address rem_address st ...
            for line in text.lines().skip(1) {
                let mut fields = line.split_whitespace();
                let _sl = fields.next();
                let Some(local) = fields.next() else { continue };
                let _rem = fields.next();
                let Some(state) = fields.next() else { continue };
                if state != "0A" {
                    // 0A = TCP_LISTEN; everything else is an established or
                    // transient socket we do not want to list.
                    continue;
                }
                if let Some((address, port)) = parse_proc_address(local) {
                    ports.push(ListeningPort {
                        proto: proto.to_string(),
                        address,
                        port,
                    });
                }
            }
        }
        ports.sort_by(|a, b| (a.port, &a.address).cmp(&(b.port, &b.address)));
        ports.dedup_by(|a, b| a.port == b.port && a.address == b.address);
        ports
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// Parses a `/proc/net/*` `ADDRESS:PORT` cell. The address is hex in the
/// kernel's per-word little-endian layout.
#[cfg(target_os = "linux")]
fn parse_proc_address(cell: &str) -> Option<(String, u16)> {
    let (addr_hex, port_hex) = cell.split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    let byte = |s: &str| u8::from_str_radix(s, 16).ok();
    let address = match addr_hex.len() {
        8 => {
            // IPv4: four bytes, least-significant first.
            let b0 = byte(&addr_hex[0..2])?;
            let b1 = byte(&addr_hex[2..4])?;
            let b2 = byte(&addr_hex[4..6])?;
            let b3 = byte(&addr_hex[6..8])?;
            format!("{b3}.{b2}.{b1}.{b0}")
        }
        32 => {
            // IPv6: four 32-bit words, each stored little-endian.
            let mut octets = [0u8; 16];
            for word in 0..4 {
                for j in 0..4 {
                    let at = word * 8 + j * 2;
                    octets[word * 4 + (3 - j)] = byte(&addr_hex[at..at + 2])?;
                }
            }
            Ipv6Addr::from(octets).to_string()
        }
        _ => return None,
    };
    Some((address, port))
}

/// Battery / power-supply summary from `/sys/class/power_supply`. Linux only.
pub fn battery_info() -> Option<BatteryInfo> {
    #[cfg(target_os = "linux")]
    {
        for entry in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
            let dir = entry.path();
            if read_trim(dir.join("type")).as_deref() != Some("Battery") {
                continue;
            }
            let percent = read_trim(dir.join("capacity")).and_then(|v| v.parse::<f32>().ok());
            let status = read_trim(dir.join("status")).unwrap_or_default();
            let charging = matches!(status.as_str(), "Charging" | "Full");
            return Some(BatteryInfo {
                percent,
                charging,
                minutes_left: battery_minutes_left(&dir),
            });
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn read_trim(path: std::path::PathBuf) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Minutes until empty, from `energy_now/power_now` (µWh/µW) or
/// `charge_now/current_now` (µAh/µA). `None` when the driver omits a rate.
#[cfg(target_os = "linux")]
fn battery_minutes_left(dir: &Path) -> Option<u64> {
    let now = read_trim(dir.join("energy_now")).or_else(|| read_trim(dir.join("charge_now")))?;
    let rate = read_trim(dir.join("power_now")).or_else(|| read_trim(dir.join("current_now")))?;
    let now = now.parse::<f64>().ok()?;
    let rate = rate.parse::<f64>().ok()?;
    if rate <= 0.0 {
        return None;
    }
    Some((now / rate * 60.0) as u64)
}

/// Maps sysinfo components to the lean snapshot temperature list.
fn temperatures_of(components: &Components) -> Vec<TemperatureInfo> {
    let mut temperatures: Vec<TemperatureInfo> = components
        .list()
        .iter()
        .map(|c| TemperatureInfo {
            label: c.label().to_string(),
            temperature_c: c.temperature(),
            max_c: c.max(),
            critical_c: c.critical(),
        })
        .collect();
    temperatures.sort_by(|a, b| a.label.cmp(&b.label));
    temperatures
}

// --- internals -------------------------------------------------------------

/// Host facts that are constant for the lifetime of a sampling session,
/// captured once instead of re-read every tick.
///
/// On Linux sysinfo's static getters re-parse `/etc/os-release` (`name`,
/// `os_version`), open `/proc` files (`kernel_version`, `host_name`) and call
/// gethostname on *every* invocation, and `Users` re-reads `/etc/passwd`.
/// None of that can change while the sampler is active, so for a 24/7
/// dashboard it is pure per-tick filesystem churn. `uid_names` is the same:
/// user names resolve once per session, not per poll.
struct HostFacts {
    hostname: Option<String>,
    os_name: Option<String>,
    os_version: Option<String>,
    kernel_version: Option<String>,
    arch: String,
    cpu_model: String,
    physical_cores: Option<usize>,
    uid_names: HashMap<Uid, String>,
}

impl HostFacts {
    fn capture(sys: &System) -> Self {
        Self {
            hostname: System::host_name(),
            os_name: System::name(),
            os_version: System::os_version(),
            kernel_version: System::kernel_version(),
            arch: std::env::consts::ARCH.to_string(),
            cpu_model: sys
                .cpus()
                .first()
                .map(|c| c.brand().trim().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            physical_cores: System::physical_core_count(),
            uid_names: uid_name_map(),
        }
    }
}

/// State kept alive between dashboard polls so CPU/process usage and
/// per-second I/O rates stay valid (sysinfo computes them as deltas between
/// refreshes of the same instance). Deliberately lean: the sampler tears this
/// whole struct down while no dashboard is open, so idle memory stays near
/// zero. The shared rolling history is owned by the sampler (see
/// `serve::HistoryRing`), not here.
pub struct WebState {
    pub sys: System,
    pub nets: Networks,
    pub disks: Disks,
    components: Components,
    facts: HostFacts,
    pub max_procs: usize,
    last: Option<std::time::Instant>,
}

impl WebState {
    pub fn new(max_procs: usize) -> Self {
        let mut sys = System::new();
        // Warm up CPU + process sampling so the dashboard's very first poll
        // already has valid CPU usage numbers and per-process thread counts.
        warmup_snapshot(&mut sys);
        Self {
            facts: HostFacts::capture(&sys),
            sys,
            nets: Networks::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
            components: Components::new_with_refreshed_list(),
            max_procs,
            last: None,
        }
    }

    /// Refresh everything and produce a lean dashboard snapshot.
    pub fn snapshot(&mut self) -> WebSnapshot {
        let now = std::time::Instant::now();
        let dt = self
            .last
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
            .max(0.001);
        self.last = Some(now);

        // Light passes: CPU + process usage + memory + thread counts. No
        // cmdline/exe/env walk, which keeps even busy servers cheap to poll.
        self.sys.refresh_cpu_all();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, lean_process_kind());
        self.sys.refresh_memory();
        self.nets.refresh(true);
        self.disks.refresh(true);
        self.components.refresh(true);

        web_snapshot(
            &self.sys,
            &self.nets,
            &self.disks,
            &self.components,
            &self.facts,
            dt,
            self.max_procs,
        )
    }
}

/// Builds a lean snapshot for the embedded dashboards (no cmdline, plus
/// per-second I/O rates derived from `dt`).
fn web_snapshot(
    sys: &System,
    nets: &Networks,
    disks: &Disks,
    components: &Components,
    facts: &HostFacts,
    dt: f64,
    max_procs: usize,
) -> WebSnapshot {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let uptime = System::uptime();
    let total_mem = sys.total_memory();
    let uid_names = &facts.uid_names;

    // Rank the raw process handles first and only materialize the top slice, so
    // the (much larger) remainder is never turned into owned name/state/user
    // strings just to be dropped by the truncate. Same order as building every
    // row and sorting after: the comparator is identical.
    let mut top: Vec<&Process> = sys.processes().values().collect();
    top.sort_by(|a, b| {
        b.cpu_usage()
            .partial_cmp(&a.cpu_usage())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    top.truncate(max_procs.max(1));

    let processes: Vec<WebProcessInfo> = top
        .into_iter()
        .map(|p| {
            let mem = p.memory();
            WebProcessInfo {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                user: resolve_user(p, uid_names),
                cpu_usage: p.cpu_usage(),
                memory: mem,
                memory_percent: if total_mem == 0 {
                    0.0
                } else {
                    mem as f32 * 100.0 / total_mem as f32
                },
                state: p.status().to_string(),
                threads: p.tasks().map(|t| t.len()),
                uptime_secs: uptime.saturating_sub(now.saturating_sub(p.start_time())),
            }
        })
        .collect();

    let networks = nets
        .list()
        .iter()
        .map(|(name, data)| WebNetworkInfo {
            name: name.clone(),
            received: data.total_received(),
            transmitted: data.total_transmitted(),
            rx_bps: data.received() as f64 / dt,
            tx_bps: data.transmitted() as f64 / dt,
        })
        .collect();

    let disks = disks
        .list()
        .iter()
        .map(|d| {
            let total = d.total_space();
            let available = d.available_space();
            let usage = d.usage();
            let name = d.name().to_string_lossy();
            WebDiskInfo {
                name: if name.is_empty() {
                    "(no label)".to_string()
                } else {
                    name.into_owned()
                },
                mount_point: d.mount_point().to_string_lossy().into_owned(),
                file_system: d.file_system().to_string_lossy().into_owned(),
                total,
                available,
                used: total.saturating_sub(available),
                read_bps: usage.read_bytes as f64 / dt,
                write_bps: usage.written_bytes as f64 / dt,
                removable: d.is_removable(),
                read_only: d.is_read_only(),
            }
        })
        .collect();

    WebSnapshot {
        ts: now,
        system: web_system_info(facts),
        cpu: build_cpu_from_facts(sys, facts),
        memory: build_memory(sys),
        disks,
        networks,
        processes,
        sensors: SensorsInfo {
            temperatures: temperatures_of(components),
        },
        listening: listening_ports(),
        battery: battery_info(),
        // Filled in by `WebState::snapshot` from the rolling history.
        history: WebHistory {
            cpu: Vec::new(),
            mem: Vec::new(),
            swap: Vec::new(),
            rx: Vec::new(),
            tx: Vec::new(),
            cores: Vec::new(),
            disks: std::collections::BTreeMap::new(),
        },
    }
}

/// Two-sample pass so both per-core CPU usage and process CPU usage have
/// a valid diff to compute against. Used by the one-shot CLI paths, which
/// render command lines, so it fetches everything (cmd/exe/cwd/environ).
fn sample_cpu_and_processes(sys: &mut System) {
    sys.refresh_cpu_all();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_all();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
}

/// Two-sample warmup for the dashboard's `WebState`, refreshing processes
/// with the *same lean kind* the tick sampler uses (CPU, memory, user,
/// tasks). The essential difference from `sample_cpu_and_processes`: cmdline,
/// executable, cwd and environment are never fetched here. Fetching them was
/// just the warmup's job, yet sysinfo keeps those heavy string fields for
/// every process on the host for the entire active session — hundreds of
/// allocations the dashboard never displays — so skipping them is the single
/// biggest lean win available, with no behavior change.
fn warmup_snapshot(sys: &mut System) {
    sys.refresh_cpu_all();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, lean_process_kind());
    thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_all();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, lean_process_kind());
}

/// Process refresh kind for dashboard sampling: enough for everything the
/// snapshot renders (CPU, memory, user names, live thread counts) and nothing
/// heavier. Notably it never fetches cmdline/exe/cwd/environ, and per-process
/// disk I/O is skipped too — the dashboard renders disk I/O per *disk*, not
/// per process, and on Linux dropping disk usage saves a `/proc/{pid}/io`
/// read per process per tick.
fn lean_process_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_user(UpdateKind::OnlyIfNotSet)
        .with_tasks()
}

fn sample_cpu(sys: &mut System) {
    sys.refresh_cpu_all();
    thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_all();
}

/// Builds a map of user id -> user name for process owner resolution.
fn uid_name_map() -> HashMap<Uid, String> {
    let users = Users::new_with_refreshed_list();
    users
        .iter()
        .map(|u| (u.id().clone(), u.name().to_string()))
        .collect()
}

fn build_cpu(sys: &System) -> CpuInfo {
    let first = sys.cpus().first();
    CpuInfo {
        model: first
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        physical_cores: System::physical_core_count(),
        logical_cores: sys.cpus().len(),
        usage: sys.global_cpu_usage(),
        cores: sys
            .cpus()
            .iter()
            .map(|c| CpuCoreInfo {
                name: c.name().to_string(),
                usage: c.cpu_usage(),
                frequency_mhz: c.frequency(),
            })
            .collect(),
    }
}

/// Dashboard CPU info: model and physical-core count come from the
/// per-session cache (they are constant), per-core usage/frequency stay live.
fn build_cpu_from_facts(sys: &System, facts: &HostFacts) -> CpuInfo {
    CpuInfo {
        model: facts.cpu_model.clone(),
        physical_cores: facts.physical_cores,
        logical_cores: sys.cpus().len(),
        usage: sys.global_cpu_usage(),
        cores: sys
            .cpus()
            .iter()
            .map(|c| CpuCoreInfo {
                name: c.name().to_string(),
                usage: c.cpu_usage(),
                frequency_mhz: c.frequency(),
            })
            .collect(),
    }
}

/// Live system info for dashboards: static facts (hostname, OS, kernel,
/// arch) come from the per-session cache, while uptime and load average stay
/// current.
#[cfg(unix)]
fn web_system_info(facts: &HostFacts) -> SystemInfo {
    let la = System::load_average();
    SystemInfo {
        hostname: facts.hostname.clone(),
        os_name: facts.os_name.clone(),
        os_version: facts.os_version.clone(),
        kernel_version: facts.kernel_version.clone(),
        arch: facts.arch.clone(),
        uptime_secs: System::uptime(),
        load_average: Some(LoadAvgInfo {
            one: la.one,
            five: la.five,
            fifteen: la.fifteen,
        }),
    }
}

#[cfg(not(unix))]
fn web_system_info(facts: &HostFacts) -> SystemInfo {
    SystemInfo {
        hostname: facts.hostname.clone(),
        os_name: facts.os_name.clone(),
        os_version: facts.os_version.clone(),
        kernel_version: facts.kernel_version.clone(),
        arch: facts.arch.clone(),
        uptime_secs: System::uptime(),
        load_average: None, // load average is not implemented on Windows
    }
}

fn build_memory(sys: &System) -> MemoryInfo {
    let total = sys.total_memory();
    let used = sys.used_memory();
    MemoryInfo {
        total,
        used,
        available: sys.available_memory(),
        free: sys.free_memory(),
        swap_total: sys.total_swap(),
        swap_used: sys.used_swap(),
        used_percent: if total == 0 {
            0.0
        } else {
            used as f32 * 100.0 / total as f32
        },
    }
}

fn build_processes(sys: &System, cli: &Cli, uid_names: &HashMap<Uid, String>) -> Vec<ProcessInfo> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let uptime = System::uptime();
    let total_mem = sys.total_memory();

    // filter
    let name_filter = cli.filter_name.as_ref().map(|f| f.to_lowercase());
    let user_filter = cli.filter_user.as_ref().map(|f| f.to_lowercase());

    let mut procs: Vec<ProcessInfo> = sys
        .processes()
        .values()
        .filter_map(|p| {
            let name = p.name().to_string_lossy().into_owned();
            if let Some(f) = &name_filter
                && !name.to_lowercase().contains(f.as_str())
            {
                return None;
            }
            let user = resolve_user(p, uid_names);
            if let Some(f) = &user_filter {
                match &user {
                    Some(u) if u.to_lowercase().contains(f.as_str()) => {}
                    _ => return None,
                }
            }
            Some(ProcessInfo {
                pid: p.pid().as_u32(),
                name,
                cpu_usage: p.cpu_usage(),
                memory: p.memory(),
                memory_percent: if total_mem == 0 {
                    0.0
                } else {
                    p.memory() as f32 * 100.0 / total_mem as f32
                },
                user,
                start_time_secs: p.start_time(),
                uptime_secs: uptime.saturating_sub(now.saturating_sub(p.start_time())),
                parent: p.parent().map(|pid| pid.as_u32()),
                threads: p.tasks().map(|t| t.len()),
                state: p.status().to_string(),
                cmdline: {
                    let cmd = p.cmd();
                    if cmd.is_empty() {
                        None
                    } else {
                        Some(
                            cmd.iter()
                                .map(|s| s.to_string_lossy().into_owned())
                                .collect::<Vec<_>>()
                                .join(" "),
                        )
                    }
                },
            })
        })
        .collect();

    sort_procs(&mut procs, cli.sort, cli.reverse);
    procs.truncate(cli.top.max(1));
    procs
}

/// Orders process rows. The one place the order is decided, so `top` and the
/// one-shot sections can never disagree.
pub fn sort_procs(procs: &mut [ProcessInfo], sort: SortKey, reverse: bool) {
    use std::cmp::Ordering;
    procs.sort_by(|a, b| {
        let ord = match sort {
            SortKey::Cpu => b
                .cpu_usage
                .partial_cmp(&a.cpu_usage)
                .unwrap_or(Ordering::Equal),
            SortKey::Mem => b.memory.cmp(&a.memory),
            SortKey::Pid => a.pid.cmp(&b.pid),
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Uptime => b.uptime_secs.cmp(&a.uptime_secs),
            SortKey::Starttime => b.start_time_secs.cmp(&a.start_time_secs),
        };
        if reverse { ord.reverse() } else { ord }
    });
}

/// One frame of `sysview top`: the header numbers and the sorted rows.
pub struct TopFrame {
    pub hostname: String,
    pub uptime_secs: u64,
    pub logical_cores: usize,
    pub cpu_usage: f32,
    pub memory: MemoryInfo,
    pub processes: Vec<ProcessInfo>,
}

/// State kept alive between `top` frames, so each CPU percentage is a delta
/// against the previous frame instead of the process's whole life.
pub struct TopState {
    sys: System,
    uid_names: HashMap<Uid, String>,
    hostname: String,
}

impl TopState {
    pub fn new() -> Self {
        let mut sys = System::new();
        warmup_snapshot(&mut sys);
        Self {
            sys,
            uid_names: uid_name_map(),
            hostname: System::host_name().unwrap_or_default(),
        }
    }

    /// Refreshes and renders nothing: the caller draws one frame per call.
    /// Sampling is the dashboard's lean pass, so a monitor left open costs
    /// about what the dashboard costs per tick.
    pub fn frame(&mut self, sort: SortKey, reverse: bool, top: usize) -> TopFrame {
        self.sys.refresh_cpu_all();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, lean_process_kind());
        self.sys.refresh_memory();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let uptime = System::uptime();
        let total_mem = self.sys.total_memory();

        let mut processes: Vec<ProcessInfo> = self
            .sys
            .processes()
            .values()
            .map(|p| ProcessInfo {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cpu_usage: p.cpu_usage(),
                memory: p.memory(),
                memory_percent: if total_mem == 0 {
                    0.0
                } else {
                    p.memory() as f32 * 100.0 / total_mem as f32
                },
                user: resolve_user(p, &self.uid_names),
                start_time_secs: p.start_time(),
                uptime_secs: uptime.saturating_sub(now.saturating_sub(p.start_time())),
                parent: p.parent().map(|pid| pid.as_u32()),
                threads: p.tasks().map(|t| t.len()),
                state: p.status().to_string(),
                cmdline: None,
            })
            .collect();
        sort_procs(&mut processes, sort, reverse);
        processes.truncate(top.max(1));

        TopFrame {
            hostname: self.hostname.clone(),
            uptime_secs: uptime,
            logical_cores: self.sys.cpus().len(),
            cpu_usage: self.sys.global_cpu_usage(),
            memory: build_memory(&self.sys),
            processes,
        }
    }
}

fn resolve_user(p: &Process, uid_names: &HashMap<Uid, String>) -> Option<String> {
    // Keyed by the id value itself: no per-process `to_string()` allocation to
    // build a lookup key (Uid is Hash + Eq on every platform).
    uid_names.get(p.user_id()?).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_lean_and_consistent() {
        let mut st = WebState::new(50);
        let s1 = st.snapshot();
        // History is sampler-owned; at this layer it stays empty.
        assert_eq!(s1.history.cpu.len(), 0);
        assert_eq!(s1.history.tx.len(), 0);
        // Process list respects the max_procs cap from a fully-populated set.
        assert!(s1.processes.len() <= 50);
        assert!(!s1.processes.is_empty());
        // Hits all the sections the dashboard renders.
        assert!(s1.system.hostname.is_some());
        assert!(!s1.networks.is_empty()); // at least one interface on every supported OS
        assert!(!s1.disks.is_empty());
        // Monotonic timestamps across a second poll.
        let s2 = st.snapshot();
        assert!(s2.ts >= s1.ts);
    }

    #[test]
    fn cpu_usage_is_valid_after_warmup() {
        // The whole point of the warmup: the very first snapshot already
        // carries a real percentage, not a zero.
        let mut st = WebState::new(20);
        let s = st.snapshot();
        assert!(
            (0.0..=100.0).contains(&s.cpu.usage),
            "cpu usage out of range: {}",
            s.cpu.usage
        );
    }

    #[test]
    fn listening_ports_are_sorted_and_deduped() {
        let ports = listening_ports();
        for w in ports.windows(2) {
            assert!(
                (w[0].port, &w[0].address) < (w[1].port, &w[1].address),
                "not strictly sorted: {:?} then {:?}",
                w[0].port,
                w[1].port
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_address_decodes_ipv4_and_ipv6() {
        assert_eq!(
            parse_proc_address("0100007F:0016"),
            Some(("127.0.0.1".to_string(), 22))
        );
        assert_eq!(
            parse_proc_address("00000000:1F90"),
            Some(("0.0.0.0".to_string(), 8080))
        );
        // :: with port 443 (0x01BB).
        assert_eq!(
            parse_proc_address("00000000000000000000000000000000:01BB"),
            Some(("::".to_string(), 443))
        );
        // Loopback ::1 in the kernel's little-endian word layout.
        assert_eq!(
            parse_proc_address("00000000000000000000000001000000:0050"),
            Some(("::1".to_string(), 80))
        );
        assert_eq!(parse_proc_address("garbage"), None);
    }
}
